use std::collections::HashMap;

use super::{Effect, Location, Participant, ParticipantId, Presence, SurfaceId};

#[derive(Debug, Clone, Default)]
pub struct Store {
    participants: HashMap<ParticipantId, Participant>,
    locations: HashMap<ParticipantId, Location>,
    presence: HashMap<SurfaceId, Vec<Presence>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("participant not found")]
pub struct MissingParticipant {
    pub id: ParticipantId,
}

impl Store {
    #[must_use]
    pub fn join(&mut self, participant: Participant) -> Vec<Effect> {
        self.participants.insert(participant.id, participant);
        Vec::new()
    }

    #[must_use]
    pub fn leave(&mut self, participant: ParticipantId) -> Vec<Effect> {
        self.participants.remove(&participant);
        self.locations.remove(&participant);
        self.presence
            .values_mut()
            .for_each(|items| items.retain(|item| item.participant != participant));
        vec![Effect::ClearPresence { participant }]
    }

    pub fn publish_location(
        &mut self,
        participant: ParticipantId,
        location: Location,
    ) -> Result<Vec<Effect>, MissingParticipant> {
        if !self.participants.contains_key(&participant) {
            return Err(MissingParticipant { id: participant });
        }
        self.locations.insert(participant, location.clone());
        Ok(vec![Effect::Open {
            participant,
            location,
        }])
    }

    pub fn clear_location(
        &mut self,
        participant: ParticipantId,
    ) -> Result<Vec<Effect>, MissingParticipant> {
        if !self.participants.contains_key(&participant) {
            return Err(MissingParticipant { id: participant });
        }
        self.locations.remove(&participant);
        Ok(Vec::new())
    }

    #[must_use]
    pub fn show_presence(&mut self, surface: SurfaceId, presence: Vec<Presence>) -> Vec<Effect> {
        self.presence.insert(surface, presence.clone());
        vec![Effect::ShowPresence { surface, presence }]
    }

    #[must_use]
    pub fn clear_presence(&mut self, surface: SurfaceId) -> Vec<Effect> {
        self.presence.remove(&surface);
        Vec::new()
    }

    pub fn participant(&self, id: ParticipantId) -> Option<&Participant> {
        self.participants.get(&id)
    }

    pub fn location(&self, id: ParticipantId) -> Option<&Location> {
        self.locations.get(&id)
    }

    pub fn locations(&self) -> impl Iterator<Item = (ParticipantId, &Location)> {
        self.locations
            .iter()
            .map(|(&participant, location)| (participant, location))
    }

    pub fn presence(&self, id: SurfaceId) -> Option<&[Presence]> {
        self.presence.get(&id).map(Vec::as_slice)
    }
}
