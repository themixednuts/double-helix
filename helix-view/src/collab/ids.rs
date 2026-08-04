//! Stable identifiers for collaboration primitives (`docs/runtime-collaboration-implementation-plan.md` Phase 5).

use crate::id::Id;
use std::num::NonZeroU64;

/// Marker for [`SurfaceId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SurfaceKind {}

/// Opaque surface identity (tabs, panels, scratch buffers tied to collab).
pub type SurfaceId = Id<SurfaceKind, NonZeroU64>;

pub use helix_collab::ParticipantId;

const ASSISTANT_PARTICIPANT_NAMESPACE: &[u8; 8] = b"dhx-asst";

pub fn assistant_participant(raw: u64) -> ParticipantId {
    let mut bytes = [0_u8; 16];
    bytes[..8].copy_from_slice(ASSISTANT_PARTICIPANT_NAMESPACE);
    bytes[8..].copy_from_slice(&raw.to_be_bytes());
    ParticipantId::from_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assistant_participants_are_stable_and_distinct() {
        assert_eq!(assistant_participant(7), assistant_participant(7));
        assert_ne!(assistant_participant(7), assistant_participant(8));
    }
}
