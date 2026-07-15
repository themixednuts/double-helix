use super::{Action, Editor};

impl Editor {
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
        let existing_document = self.document_id_by_path(&location.path);
        let target_view = location
            .surface
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
            path: location.path.clone(),
            action: Action::Replace,
            target: target_view,
            range: location
                .range
                .map(|range| helix_core::Range::new(range.anchor, range.head)),
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
    use crate::collab::{participant, Participant, ParticipantId};
    use crate::editor::test_support;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn participant(id: u64, name: &str) -> Participant {
        Participant {
            id: ParticipantId::new(std::num::NonZeroU64::new(id).unwrap()),
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
        assert!(editor.document_id_by_path(&request.path).is_none());
        let _ = fs::remove_file(new_path);
    }
}
