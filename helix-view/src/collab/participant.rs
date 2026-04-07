//! Participants (`docs/runtime-collaboration-implementation-plan.md` Phase 5).

use super::ParticipantId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Agent,
    Human,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Observe,
    Read,
    Write,
}

/// Collaboration participant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Participant {
    pub id: ParticipantId,
    pub kind: Kind,
    pub name: String,
    pub access: Access,
}
