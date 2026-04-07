//! Presence updates (Phase 5).

use super::location::{RangeAnchor, ViewportAnchor};
use super::{ParticipantId, SurfaceId};

/// Presence state for a participant on a surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Presence {
    pub participant: ParticipantId,
    pub surface: SurfaceId,
    pub cursor: Option<RangeAnchor>,
    pub selection: Option<RangeAnchor>,
    pub viewport: Option<ViewportAnchor>,
}
