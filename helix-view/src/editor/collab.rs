use super::{Action, Editor};
use crate::file_bound::DocumentLocation;
use helix_core::Transaction;

impl Editor {
    pub fn attach_collaboration_session(
        &mut self,
        session: helix_collab::GuestSessionHandle,
        hosting: bool,
    ) {
        let participants = session.project().participants.clone();
        self.collaboration.attach(session, hosting);
        for participant in participants {
            self.join_participant(crate::collab::Participant {
                id: participant.id,
                kind: crate::collab::participant::Kind::Human,
                name: participant.name,
                access: participant.role,
            });
        }
    }

    pub fn detach_collaboration_session(&mut self) -> Option<helix_collab::GuestSessionHandle> {
        for document in self.collaboration.documents() {
            if let Some(doc) = self.document_mut(document) {
                doc.set_collaboration_role(None);
            }
        }
        self.collaboration.detach()
    }

    pub fn bind_collaboration_buffer(
        &mut self,
        document: crate::DocumentId,
        buffer: helix_collab::BufferId,
        path: helix_workspace::WorkspacePath,
    ) -> bool {
        let role = self.collaboration.role();
        let previous = self
            .collaboration
            .buffer(document)
            .filter(|previous| *previous != buffer);
        let Some(doc) = self.document_mut(document) else {
            return false;
        };
        doc.set_collaboration_role(role);
        if let Some(previous) = previous {
            if let Some(session) = self.collaboration.session() {
                session.release(previous);
            }
        }
        self.collaboration.bind(document, buffer, path);
        let mut changed = true;
        if let Some(presence) = self.collaboration.take_presence(buffer) {
            changed |= self.apply_collaboration_presence(presence);
        }
        changed
    }

    pub fn unbind_collaboration_document(&mut self, document: crate::DocumentId) {
        let buffer = self.collaboration.buffer(document);
        let session = self.collaboration.session();
        if let Some(doc) = self.document_mut(document) {
            doc.set_collaboration_role(None);
        }
        self.collaboration.unbind_document(document);
        if let (Some(session), Some(buffer)) = (session, buffer) {
            session.release(buffer);
        }
    }

    fn collaboration_view(&self, document: crate::DocumentId) -> Option<crate::ViewId> {
        self.tree
            .views()
            .filter(|(view, _)| view.doc == document)
            .min_by_key(|(_, focused)| !*focused)
            .map(|(view, _)| view.id)
    }

    fn apply_collaboration_text_changes(
        &mut self,
        buffer: helix_collab::BufferId,
        changes: Vec<helix_collab::TextChange>,
    ) -> bool {
        if changes.is_empty() {
            return false;
        }
        let Some(document) = self.collaboration.document(buffer) else {
            return false;
        };
        let view = self.collaboration_view(document);
        let Some(doc) = self.document_mut(document) else {
            self.unbind_collaboration_document(document);
            return false;
        };
        let mut len = doc.text().len_chars();
        for change in &changes {
            if change.start > change.end || change.end > len {
                log::warn!(
                    "discarding invalid collaboration change: buffer={buffer:?} start={} end={} len={len}",
                    change.start,
                    change.end,
                );
                return false;
            }
            len = len - (change.end - change.start) + change.insert.chars().count();
        }
        changes.into_iter().fold(false, |changed, change| {
            let transaction = Transaction::change(
                doc.text(),
                std::iter::once((change.start, change.end, Some(change.insert.into()))),
            );
            doc.apply_collaboration(&transaction, view) || changed
        })
    }

    fn apply_collaboration_snapshot(
        &mut self,
        buffer: helix_collab::BufferId,
        text: String,
    ) -> bool {
        let Some(document) = self.collaboration.document(buffer) else {
            return false;
        };
        let view = self.collaboration_view(document);
        let Some(doc) = self.document_mut(document) else {
            self.unbind_collaboration_document(document);
            return false;
        };
        if *doc.text() == text {
            return false;
        }
        let transaction = Transaction::change(
            doc.text(),
            std::iter::once((0, doc.text().len_chars(), Some(text.into()))),
        );
        doc.apply_collaboration(&transaction, view)
    }

    fn apply_collaboration_buffer_saved(
        &mut self,
        buffer: helix_collab::BufferId,
        version: helix_collab::FileVersion,
    ) -> bool {
        let Some(document) = self.collaboration.document(buffer) else {
            return false;
        };
        let Some(doc) = self.document_mut(document) else {
            self.unbind_collaboration_document(document);
            return false;
        };
        if doc.remote_location().is_none() {
            return false;
        }
        match helix_collab::RemoteBackend::decode_file_version(&version) {
            Ok(content) => doc.set_remote_content(content),
            Err(error) => {
                log::error!("host returned an invalid remote save generation: {error}");
                false
            }
        }
    }

    fn apply_collaboration_role(
        &mut self,
        participant: crate::collab::ParticipantId,
        role: helix_collab::Role,
    ) -> bool {
        let mut changed = self.collab.set_access(participant, role).unwrap_or(false);
        if self.collaboration.set_role(participant, role) {
            for document in self.collaboration.documents() {
                if let Some(doc) = self.document_mut(document) {
                    doc.set_collaboration_role(Some(role));
                }
            }
            changed = true;
        }
        changed
    }

    fn set_collaboration_connection_role(&mut self, role: Option<helix_collab::Role>) {
        self.collaboration.set_connection_role(role);
        for document in self.collaboration.documents() {
            if let Some(doc) = self.document_mut(document) {
                doc.set_collaboration_role(role);
            }
        }
    }

    fn set_collaboration_document_path(
        &mut self,
        document: crate::DocumentId,
        project: helix_collab::ProjectId,
        path: helix_workspace::WorkspacePath,
    ) -> bool {
        let Some(previous) = self.collaboration.path(document) else {
            return false;
        };
        if previous == path {
            return false;
        }
        let Some(location) = self
            .document(document)
            .and_then(|document| document.location())
            .cloned()
        else {
            self.unbind_collaboration_document(document);
            return false;
        };
        let changed = match location {
            DocumentLocation::Collaboration(location) => {
                if location.project != project {
                    return false;
                }
                match self
                    .document_mut(document)
                    .expect("bound collaboration document exists")
                    .set_collaboration_path(path.clone())
                {
                    Ok(changed) => changed,
                    Err(error) => {
                        log::error!(
                            "failed to update collaboration document resource URL: {error}"
                        );
                        return false;
                    }
                }
            }
            DocumentLocation::Remote(_) if self.collaboration.is_hosting() => {
                match self.set_doc_remote_path(document, path.clone()) {
                    Ok(changed) => changed,
                    Err(error) => {
                        log::error!("failed to update remote document resource URL: {error}");
                        return false;
                    }
                }
            }
            DocumentLocation::Local(current) if self.collaboration.is_hosting() => {
                let previous_relative = previous.to_path_buf();
                let next_relative = path.to_path_buf();
                let destination = if current.ends_with(&next_relative) {
                    current.clone()
                } else if current.ends_with(&previous_relative) {
                    let mut root = current.clone();
                    for _ in previous.segments() {
                        if !root.pop() {
                            log::error!(
                                "bound local document path has no workspace root: {}",
                                current.display()
                            );
                            return false;
                        }
                    }
                    root.join(next_relative)
                } else {
                    log::error!(
                        "bound local document escaped its collaboration path: document={} binding={previous}",
                        current.display()
                    );
                    return false;
                };
                let changed = current != destination;
                if changed {
                    self.set_doc_path(document, &destination);
                }
                changed
            }
            DocumentLocation::Local(_) | DocumentLocation::Remote(_) => return false,
        };
        self.collaboration.set_path(document, path);
        changed
    }

    fn apply_collaboration_file_transaction(
        &mut self,
        transaction: helix_workspace::FileTransaction,
        undone: bool,
    ) -> bool {
        use helix_workspace::FileOperation;

        let project = self
            .collaboration
            .session()
            .map(|session| session.project().id);
        let Some(project) = project else {
            return false;
        };
        let operations: Box<dyn Iterator<Item = &FileOperation>> = if undone {
            Box::new(transaction.operations.iter().rev())
        } else {
            Box::new(transaction.operations.iter())
        };
        let mut changed = false;
        for operation in operations {
            let FileOperation::Rename { from, to, .. } = operation else {
                continue;
            };
            let (from, to) = if undone { (to, from) } else { (from, to) };
            for document in self.collaboration.documents() {
                let Some(current) = self.collaboration.path(document) else {
                    continue;
                };
                let Some(relative) = current.strip_prefix(from) else {
                    continue;
                };
                let Ok(path) = to.join_path(&relative) else {
                    log::error!("host sent an invalid renamed path: from={from} to={to}");
                    continue;
                };
                changed |= self.set_collaboration_document_path(document, project, path);
            }
        }
        changed
    }

    fn apply_collaboration_project_state(&mut self, state: helix_collab::ProjectState) -> bool {
        let Some(project) = self
            .collaboration
            .session()
            .map(|session| session.project().id)
        else {
            return false;
        };
        let mut changed = false;
        for participant in self.collaboration.replace_participants(&state.participants) {
            if self.collaboration.following() == Some(participant) {
                self.collaboration.set_following(None);
            }
            let effects = self.leave_participant(participant);
            self.apply_collab_effects(effects);
            changed = true;
        }
        for participant in state.participants {
            let current = crate::collab::Participant {
                id: participant.id,
                kind: crate::collab::participant::Kind::Human,
                name: participant.name,
                access: participant.role,
            };
            if self.participant(current.id) != Some(&current) {
                self.join_participant(current.clone());
                changed = true;
            }
            changed |= self.apply_collaboration_role(current.id, current.access);
        }
        for open in state.open_buffers {
            let Some(document) = self.collaboration.document(open.buffer) else {
                continue;
            };
            changed |= self.set_collaboration_document_path(document, project, open.path);
        }
        changed
    }

    fn apply_collaboration_presence(&mut self, presence: helix_collab::ResolvedPresence) -> bool {
        let Some(document) = self.collaboration.document(presence.buffer) else {
            if self.collaboration.following() == Some(presence.participant) {
                self.collaboration.queue_presence(presence);
            }
            return false;
        };
        let Some(view) = self.collaboration_view(document) else {
            return false;
        };
        let Some(surface) = self.track_tree_surface(view) else {
            return false;
        };
        let Some(doc) = self.document(document) else {
            return false;
        };
        let len = doc.text().len_chars();
        let followed_selection = presence.selection.or_else(|| {
            presence
                .cursor
                .map(|cursor| (cursor.min(len), cursor.min(len)))
        });
        let followed_viewport = presence.viewport.map(|viewport| viewport.min(len));
        let should_follow = self.collaboration.following() == Some(presence.participant);
        let mut current = self.collab.presence(surface).unwrap_or_default().to_vec();
        current.retain(|item| item.participant != presence.participant);
        current.push(crate::collab::Presence {
            participant: presence.participant,
            surface,
            cursor: presence.cursor.map(|position| {
                crate::collab::RangeAnchor::new(position.min(len), position.min(len))
            }),
            selection: presence.selection.map(|(anchor, head)| {
                crate::collab::RangeAnchor::new(anchor.min(len), head.min(len))
            }),
            viewport: presence
                .viewport
                .map(|anchor| crate::collab::ViewportAnchor::new(anchor.min(len), 0, 0)),
        });
        self.apply_presence(surface, current);
        if should_follow {
            let view_state = self.tree.get(view).clone();
            let scrolloff = self.config().scrolloff;
            let collaboration = self.collaboration.clone();
            if let Some(doc) = self.document_mut(document) {
                if let Some((anchor, head)) = followed_selection {
                    doc.set_collaboration_selection(
                        view,
                        helix_core::Selection::single(anchor, head),
                    );
                }
                if let Some(anchor) = followed_viewport {
                    let mut offset = doc.view_offset(view);
                    offset.anchor = anchor;
                    doc.set_view_offset(view, offset);
                }
                view_state.ensure_cursor_in_view(doc, scrolloff);
                collaboration.record_followed_presence(doc, view);
            }
        }
        true
    }

    pub fn apply_collaboration_update(&mut self, update: helix_collab::GuestSessionUpdate) {
        let changed = match update {
            helix_collab::GuestSessionUpdate::TextChanged { buffer, changes } => {
                self.apply_collaboration_text_changes(buffer, changes)
            }
            helix_collab::GuestSessionUpdate::Snapshot { buffer, text } => {
                self.apply_collaboration_snapshot(buffer, text)
            }
            helix_collab::GuestSessionUpdate::ParticipantJoined(participant) => {
                self.collaboration.participant_joined(participant.id);
                self.join_participant(crate::collab::Participant {
                    id: participant.id,
                    kind: crate::collab::participant::Kind::Human,
                    name: participant.name,
                    access: participant.role,
                });
                true
            }
            helix_collab::GuestSessionUpdate::ParticipantLeft(participant) => {
                self.collaboration.participant_left(participant);
                if self.collaboration.following() == Some(participant) {
                    self.collaboration.set_following(None);
                }
                let effects = self.leave_participant(participant);
                self.apply_collab_effects(effects);
                true
            }
            helix_collab::GuestSessionUpdate::RoleChanged { participant, role } => {
                self.apply_collaboration_role(participant, role)
            }
            helix_collab::GuestSessionUpdate::Presence(presence) => {
                self.apply_collaboration_presence(presence)
            }
            helix_collab::GuestSessionUpdate::PresenceCleared {
                participant,
                buffer,
            } => {
                self.collaboration.clear_presence(buffer);
                if self.collaboration.following() == Some(participant) {
                    self.collaboration.set_following(None);
                }
                let effects = self.collab.clear_participant_presence(participant);
                self.apply_collab_effects(effects);
                true
            }
            helix_collab::GuestSessionUpdate::FollowRequested { follower, leader } => {
                if self.collaboration.participant() == Some(leader) {
                    let name = self
                        .participant(follower)
                        .map(|participant| participant.name.as_str())
                        .unwrap_or("A participant");
                    self.set_status(format!("{name} is following you"));
                    true
                } else {
                    false
                }
            }
            helix_collab::GuestSessionUpdate::Following {
                participant,
                location,
            } => {
                self.collaboration.set_following(Some(participant));
                if let Some(location) = location {
                    let buffer = location.presence.buffer;
                    if self.collaboration.document(buffer).is_some() {
                        self.apply_collaboration_presence(location.presence);
                    } else {
                        self.collaboration.queue_presence(location.presence.clone());
                        self.request_collaboration_reveal(location.path, &location.presence);
                    }
                }
                true
            }
            helix_collab::GuestSessionUpdate::BufferSaved { buffer, version } => {
                self.apply_collaboration_buffer_saved(buffer, version)
            }
            helix_collab::GuestSessionUpdate::ProjectState(state) => {
                self.apply_collaboration_project_state(state)
            }
            helix_collab::GuestSessionUpdate::FilesChanged {
                transaction,
                undone,
            } => self.apply_collaboration_file_transaction(transaction, undone),
            helix_collab::GuestSessionUpdate::WorktreeChanged { .. } => false,
            helix_collab::GuestSessionUpdate::LanguageServerDiagnostics(_) => false,
            helix_collab::GuestSessionUpdate::LanguageServerRefresh(_) => false,
            helix_collab::GuestSessionUpdate::Connection(state) => match state {
                helix_collab::ConnectionState::Connected(participant) => {
                    self.collaboration.participant_joined(participant.id);
                    self.join_participant(crate::collab::Participant {
                        id: participant.id,
                        kind: crate::collab::participant::Kind::Human,
                        name: participant.name,
                        access: participant.role,
                    });
                    self.set_collaboration_connection_role(Some(participant.role));
                    self.apply_collaboration_role(participant.id, participant.role);
                    self.set_status("Collaboration connected");
                    true
                }
                helix_collab::ConnectionState::Reconnecting { attempt } => {
                    self.set_status(format!("Collaboration reconnecting (attempt {attempt})"));
                    true
                }
                helix_collab::ConnectionState::Failed(error) => {
                    self.collaboration.set_following(None);
                    self.set_collaboration_connection_role(None);
                    self.notify_error(format!("Collaboration connection failed: {error}"));
                    true
                }
                helix_collab::ConnectionState::Closed => {
                    self.collaboration.set_following(None);
                    self.set_collaboration_connection_role(None);
                    self.set_status("Collaboration connection closed");
                    true
                }
            },
            helix_collab::GuestSessionUpdate::Error(error) => {
                self.notify_error(format!("Collaboration error: {error}"));
                true
            }
        };
        if changed {
            self.mark_redraw_pending();
            self.request_redraw();
        }
    }

    pub fn publish_location(
        &mut self,
        participant: crate::collab::ParticipantId,
        location: crate::collab::Location,
    ) -> Result<Vec<crate::collab::Effect>, crate::collab::MissingParticipant> {
        let location = self.resolve_location_surface(location);
        self.collab.publish_location(participant, location)
    }

    pub fn apply_collab_effects(&mut self, effects: Vec<crate::collab::Effect>) {
        let mut sync_presence = false;
        let mut reveals = Vec::new();

        for effect in effects {
            match effect {
                crate::collab::Effect::Open { .. }
                | crate::collab::Effect::ClearPresence { .. } => {
                    sync_presence = true;
                }
                crate::collab::Effect::Reveal { location, .. } => {
                    sync_presence = true;
                    reveals.push(location);
                }
                crate::collab::Effect::ShowPresence { surface, presence } => {
                    self.render_presence(surface, &presence);
                }
            }
        }

        if sync_presence {
            self.sync_collab_presence();
        }

        for location in reveals {
            self.request_location_reveal(
                &location,
                crate::handlers::NavigationPurpose::CollaborationReveal,
            );
        }
    }

    pub fn participant(
        &self,
        participant: crate::collab::ParticipantId,
    ) -> Option<&crate::collab::Participant> {
        self.collab.participant(participant)
    }

    pub fn collaboration_participants(&self) -> impl Iterator<Item = &crate::collab::Participant> {
        self.collab.participants()
    }

    pub fn join_participant(
        &mut self,
        participant: crate::collab::Participant,
    ) -> Vec<crate::collab::Effect> {
        self.collab.join(participant)
    }

    pub fn leave_participant(
        &mut self,
        participant: crate::collab::ParticipantId,
    ) -> Vec<crate::collab::Effect> {
        self.collab.leave(participant)
    }

    fn resolve_location_surface(
        &self,
        mut location: crate::collab::Location,
    ) -> crate::collab::Location {
        if location.surface.is_none() {
            location.surface = self.surface_for_location(&location);
        }
        location
    }

    fn surface_for_location(
        &self,
        location: &crate::collab::Location,
    ) -> Option<crate::collab::SurfaceId> {
        if let Some(surface) = location
            .surface
            .filter(|id| self.surface_registry.get(*id).is_some())
        {
            return Some(surface);
        }

        let doc_id = self.document_id_by_path(&location.path)?;
        self.surface_registry
            .surfaces()
            .filter(|surface| surface.doc == doc_id)
            .min_by_key(|surface| match surface.role {
                crate::collab::surface::Role::Editor => 0,
                crate::collab::surface::Role::Auxiliary => 1,
            })
            .map(|surface| surface.id)
    }

    fn snapshot_presence(
        &self,
        participant: crate::collab::ParticipantId,
        location: &crate::collab::Location,
    ) -> Option<crate::collab::Presence> {
        let surface = self.surface_for_location(location)?;
        let viewport = self
            .with_surface(surface, |surface_ref| match surface_ref {
                crate::collab::surface::Ref::Tree { view, doc } => {
                    let offset = doc.view_offset(view.id);
                    crate::collab::ViewportAnchor::new(
                        location
                            .range
                            .map(|range| range.head)
                            .unwrap_or(offset.anchor),
                        offset.vertical_offset,
                        offset.horizontal_offset,
                    )
                }
                crate::collab::surface::Ref::Component { view, doc } => {
                    let offset = doc.view_offset(view.id);
                    crate::collab::ViewportAnchor::new(
                        location
                            .range
                            .map(|range| range.head)
                            .unwrap_or(offset.anchor),
                        offset.vertical_offset,
                        offset.horizontal_offset,
                    )
                }
            })
            .ok();

        let cursor = location
            .range
            .map(|range| crate::collab::RangeAnchor::new(range.head, range.head));
        let selection = location.range.filter(|range| range.anchor != range.head);

        Some(crate::collab::Presence {
            participant,
            surface,
            cursor,
            selection,
            viewport,
        })
    }

    fn derived_presence_for_surface(
        &self,
        surface: crate::collab::SurfaceId,
    ) -> Vec<crate::collab::Presence> {
        self.collab
            .locations()
            .filter_map(|(participant, location)| self.snapshot_presence(participant, location))
            .filter(|presence| presence.surface == surface)
            .collect()
    }

    fn render_presence(
        &mut self,
        surface: crate::collab::SurfaceId,
        presence: &[crate::collab::Presence],
    ) {
        let annotations = crate::collab::surface::presence_annotations(self, presence);
        let _ = self.with_surface_mut(surface, |surface_ref| match surface_ref {
            crate::collab::surface::Mut::Tree { view, doc } => {
                doc.set_presence_annotations(view.id, annotations.clone());
            }
            crate::collab::surface::Mut::Component { view, doc } => {
                doc.set_presence_annotations(view.id, annotations.clone());
            }
        });
    }

    fn clear_surface_presence(&mut self, surface: crate::collab::SurfaceId) {
        let _ = self.collab.clear_presence(surface);
        self.render_presence(surface, &[]);
    }

    fn sync_collab_presence(&mut self) {
        let surfaces: Vec<_> = self
            .surface_registry
            .surfaces()
            .map(|surface| surface.id)
            .collect();
        let snapshots: Vec<_> = surfaces
            .iter()
            .copied()
            .map(|surface| (surface, self.derived_presence_for_surface(surface)))
            .collect();

        for (surface, presence) in snapshots {
            if presence.is_empty() {
                self.clear_surface_presence(surface);
            } else {
                let _ = self.collab.show_presence(surface, presence.clone());
                self.render_presence(surface, &presence);
            }
        }
    }

    pub(crate) fn request_location_reveal(
        &mut self,
        location: &crate::collab::Location,
        purpose: crate::handlers::NavigationPurpose,
    ) {
        self.request_workspace_location_reveal(
            super::WorkspaceDocumentPath::Local(location.path.clone()),
            location.range,
            location.surface,
            purpose,
        );
    }

    fn request_collaboration_reveal(
        &mut self,
        path: helix_workspace::WorkspacePath,
        presence: &helix_collab::ResolvedPresence,
    ) {
        let Some(project) = self
            .collaboration
            .session()
            .map(|session| session.project().id)
        else {
            return;
        };
        let range = presence
            .selection
            .or_else(|| presence.cursor.map(|cursor| (cursor, cursor)))
            .map(|(anchor, head)| crate::collab::RangeAnchor::new(anchor, head));
        self.request_workspace_location_reveal(
            super::WorkspaceDocumentPath::Collaboration { project, path },
            range,
            None,
            crate::handlers::NavigationPurpose::CollaborationReveal,
        );
    }

    fn request_workspace_location_reveal(
        &mut self,
        path: super::WorkspaceDocumentPath,
        range: Option<crate::collab::RangeAnchor>,
        surface: Option<crate::collab::SurfaceId>,
        purpose: crate::handlers::NavigationPurpose,
    ) {
        let existing_document = self.document_id_by_workspace_path(&path);
        let target_view = surface
            .and_then(|id| self.surface_registry.get(id))
            .map(|surface| surface.view)
            .filter(|view_id| self.tree.contains(*view_id))
            .or_else(|| {
                existing_document.and_then(|document| {
                    self.tree
                        .views()
                        .find(|(view, _)| view.doc == document)
                        .map(|(view, _)| view.id)
                })
            })
            .unwrap_or(self.tree.focus);
        let request = crate::handlers::NavigationRequest {
            path,
            action: Action::Replace,
            target: target_view,
            range: range.map(|range| helix_core::Range::new(range.anchor, range.head)),
            purpose,
        };
        if let Err(error) = self.handlers.navigation.try_send(request) {
            log::warn!(
                "dropping location reveal because navigation ingress is unavailable: {error:?}"
            );
            self.notify_warning("Could not reveal location because navigation is busy");
        }
    }

    pub fn complete_location_reveal(&mut self, assistant_follow: bool) {
        if assistant_follow {
            self.assistant_follow.suppress_pause = true;
        }
        self.sync_collab_presence();
    }

    pub fn apply_presence(
        &mut self,
        surface: crate::collab::SurfaceId,
        presence: Vec<crate::collab::Presence>,
    ) -> Vec<crate::collab::Effect> {
        let effects = self.collab.show_presence(surface, presence.clone());
        self.render_presence(surface, &presence);
        effects
    }
}

#[cfg(test)]
mod tests {
    use crate::collab::{participant, Participant};
    use crate::editor::test_support;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn participant(id: u64, name: &str) -> Participant {
        Participant {
            id: crate::collab::ids::assistant_participant(id),
            kind: participant::Kind::Agent,
            name: name.to_string(),
            access: participant::Access::Read,
        }
    }

    fn temp_file(name: &str, contents: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("helix-collab-{name}-{stamp}.rs"));
        fs::write(&path, contents).expect("write temp file");
        helix_stdx::path::canonicalize(path)
    }

    #[test]
    fn collab_effects_publish_presence_for_open_locations() {
        let mut editor = test_support::collab_test_editor();
        let alice = participant(1, "alice");
        let bob = participant(2, "bob");

        let join_alice = editor.join_participant(alice.clone());
        editor.apply_collab_effects(join_alice);
        let join_bob = editor.join_participant(bob.clone());
        editor.apply_collab_effects(join_bob);

        let alice_location = test_support::collab_test_location(&editor, alice.id, 2..5);
        let alice_effects = editor
            .publish_location(alice.id, alice_location)
            .expect("location");
        editor.apply_collab_effects(alice_effects);

        let bob_location = test_support::collab_test_location(&editor, bob.id, 8..8);
        let bob_effects = editor
            .publish_location(bob.id, bob_location)
            .expect("location");
        editor.apply_collab_effects(bob_effects);

        let surface = editor
            .surface_registry
            .get_by_view(editor.tree.focus)
            .expect("surface");
        let presence = editor.collab.presence(surface).expect("presence");
        assert_eq!(presence.len(), 2);
        assert!(presence
            .iter()
            .any(|item| item.participant == alice.id && item.selection.is_some()));
        assert!(presence
            .iter()
            .any(|item| item.participant == bob.id && item.cursor.is_some()));
    }

    #[test]
    fn surface_resolution_prefers_editor_role_over_auxiliary() {
        let mut editor = test_support::collab_test_editor();
        let alice = participant(1, "alice");

        let join_effects = editor.join_participant(alice.clone());
        editor.apply_collab_effects(join_effects);

        let editor_view = editor.tree.focus;
        let editor_surface = editor
            .surface_registry
            .get_by_view(editor_view)
            .expect("editor surface");
        let doc_id = editor.tree.get(editor_view).doc;
        let path = test_support::collab_test_path(&editor);
        let doc = doc_mut!(editor, &doc_id);
        doc.set_path(Some(&path));

        let component_view_id = editor.allocate_view_id();
        editor.ensure_component_view(component_view_id, doc_id);
        let auxiliary_surface = editor
            .surface_registry
            .get_by_view(component_view_id)
            .expect("auxiliary surface");
        assert_ne!(editor_surface, auxiliary_surface);

        let mut location = test_support::collab_test_location(&editor, alice.id, 4..9);
        location.surface = None;

        let effects = editor
            .publish_location(alice.id, location)
            .expect("location");
        editor.apply_collab_effects(effects);

        let presence = editor.collab.presence(editor_surface).expect("presence");
        assert_eq!(presence.len(), 1);
        assert_eq!(presence[0].surface, editor_surface);
        assert!(editor
            .collab
            .presence(auxiliary_surface)
            .is_none_or(|items| items.is_empty()));
    }

    #[test]
    fn leaving_participant_clears_derived_presence() {
        let mut editor = test_support::collab_test_editor();
        let alice = participant(1, "alice");

        let join_effects = editor.join_participant(alice.clone());
        editor.apply_collab_effects(join_effects);

        let location = test_support::collab_test_location(&editor, alice.id, 3..7);
        let location_effects = editor
            .publish_location(alice.id, location)
            .expect("location");
        editor.apply_collab_effects(location_effects);

        let surface = editor
            .surface_registry
            .get_by_view(editor.tree.focus)
            .expect("surface");
        assert!(editor.collab.presence(surface).is_some());

        let leave_effects = editor.leave_participant(alice.id);
        editor.apply_collab_effects(leave_effects);

        let presence = editor.collab.presence(surface).unwrap_or(&[]);
        assert!(presence.is_empty());

        let view = editor.tree.get(editor.tree.focus);
        let doc = editor.document(view.doc).expect("doc");
        let annotations = doc
            .presence_annotations(view.id)
            .cloned()
            .unwrap_or_default();
        assert!(annotations.is_empty());
    }

    #[test]
    fn collaboration_text_batches_apply_in_sequential_character_offsets() {
        let mut editor = test_support::collab_test_editor();
        let document = editor.tree.get(editor.tree.focus).doc;
        let buffer = helix_collab::BufferId(7);
        let path = helix_workspace::WorkspacePath::from_slash_path("src/main.rs").unwrap();
        assert!(editor.bind_collaboration_buffer(document, buffer, path));
        editor.apply_collaboration_update(helix_collab::GuestSessionUpdate::Snapshot {
            buffer,
            text: "a😀c".to_owned(),
        });
        editor.apply_collaboration_update(helix_collab::GuestSessionUpdate::TextChanged {
            buffer,
            changes: vec![
                helix_collab::TextChange {
                    start: 1,
                    end: 1,
                    insert: "界".to_owned(),
                },
                helix_collab::TextChange {
                    start: 2,
                    end: 3,
                    insert: String::new(),
                },
            ],
        });
        assert_eq!(
            editor.document(document).unwrap().text().to_string(),
            "a界c"
        );
    }

    #[test]
    fn collab_open_keeps_current_focus_while_loading_target_document() {
        let mut editor = test_support::collab_test_editor();
        let active_doc = editor.tree.get(editor.tree.focus).doc;
        let alice = participant(1, "alice");

        let join_effects = editor.join_participant(alice.clone());
        editor.apply_collab_effects(join_effects);
        let new_path = temp_file("open-target", "fn open_target() {}\n");

        let location =
            crate::collab::Location::new(new_path.clone(), crate::collab::location::Source::Tool)
                .with_range(crate::collab::RangeAnchor::new(0, 0));
        editor.apply_collab_effects(vec![crate::collab::Effect::Open {
            participant: alice.id,
            location,
        }]);

        assert_eq!(editor.tree.get(editor.tree.focus).doc, active_doc);
        let opened_doc = editor
            .open(&new_path, crate::editor::Action::Load)
            .expect("open target");
        assert!(editor.document(opened_doc).is_some());
        assert_eq!(editor.tree.get(editor.tree.focus).doc, active_doc);
        let _ = fs::remove_file(new_path);
    }

    #[tokio::test]
    async fn collab_reveal_emits_navigation_intent_without_opening_on_the_ui_thread() {
        let mut editor = test_support::collab_test_editor();
        let active_doc = editor.tree.get(editor.tree.focus).doc;
        let target_view = editor.tree.focus;
        let (navigation, mut navigation_rx) = helix_runtime::channel(4);
        editor.handlers.navigation = navigation;
        let alice = participant(1, "alice");

        let join_effects = editor.join_participant(alice.clone());
        editor.apply_collab_effects(join_effects);
        let new_path = temp_file("reveal-target", "fn reveal_target() {}\n");

        let location =
            crate::collab::Location::new(new_path.clone(), crate::collab::location::Source::Tool)
                .with_range(crate::collab::RangeAnchor::new(0, 0));
        editor.apply_collab_effects(vec![crate::collab::Effect::Reveal {
            participant: alice.id,
            location,
        }]);

        let request = navigation_rx.recv().await.expect("navigation request");
        assert_eq!(request.path, new_path);
        assert_eq!(request.target, target_view);
        assert_eq!(
            request.purpose,
            crate::handlers::NavigationPurpose::CollaborationReveal
        );
        assert_eq!(editor.tree.get(editor.tree.focus).doc, active_doc);
        assert!(editor
            .document_id_by_workspace_path(&request.path)
            .is_none());
        let _ = fs::remove_file(new_path);
    }
}
