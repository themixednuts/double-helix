//! Generic collaboration substrate (`docs/collaboration-assistant-architecture-spec.md`).

pub mod effect;
pub mod follow;
pub mod ids;
pub mod location;
pub mod participant;
pub mod presence;
pub mod registry;
pub mod store;
pub mod surface;

pub use effect::Effect;
pub use follow::{Mode as FollowMode, Pause as FollowPause, State as FollowState};
pub use ids::{ParticipantId, ParticipantKind, SurfaceId, SurfaceKind};
pub use location::{Location, RangeAnchor, ViewportAnchor};
pub use participant::Participant;
pub use presence::Presence;
pub use registry::Registry;
pub use store::{MissingParticipant, Store};
pub use surface::Surface;
