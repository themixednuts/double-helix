//! Stable identifiers for collaboration primitives (`docs/runtime-collaboration-implementation-plan.md` Phase 5).

use crate::id::Id;
use std::num::NonZeroU64;

/// Marker for [`SurfaceId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SurfaceKind {}

/// Opaque surface identity (tabs, panels, scratch buffers tied to collab).
pub type SurfaceId = Id<SurfaceKind, NonZeroU64>;

/// Marker for [`ParticipantId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParticipantKind {}

/// Collaboration participant (human or agent).
pub type ParticipantId = Id<ParticipantKind, NonZeroU64>;
