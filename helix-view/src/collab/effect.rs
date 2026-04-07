use super::{Location, ParticipantId, Presence, SurfaceId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    Open {
        participant: ParticipantId,
        location: Location,
    },
    Reveal {
        participant: ParticipantId,
        location: Location,
    },
    ShowPresence {
        surface: SurfaceId,
        presence: Vec<Presence>,
    },
    ClearPresence {
        participant: ParticipantId,
    },
}
