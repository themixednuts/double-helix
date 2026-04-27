//! Opaque handle types for the host-agnostic plugin contract.
//!
//! Handles are lightweight, `Copy`, serializable identity tokens. They do not
//! carry mutable state — all mutations go through request types and capability
//! traits. A handle may become stale if the underlying resource is closed;
//! APIs that accept handles must return [`super::errors::PluginError::StaleHandle`]
//! in that case.

use std::num::NonZeroU64;

use serde::{Deserialize, Serialize};

/// Define an opaque handle type backed by `NonZeroU64`.
///
/// Each handle is `Copy + Eq + Hash + Serialize + Deserialize` and carries no
/// runtime state beyond its identity value.
macro_rules! define_handle {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[repr(transparent)]
        pub struct $name(NonZeroU64);

        impl $name {
            /// Wrap a raw non-zero identity value.
            pub const fn from_raw(id: NonZeroU64) -> Self {
                Self(id)
            }

            /// Extract the raw identity value.
            pub const fn raw(self) -> NonZeroU64 {
                self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0)
            }
        }
    };
}

define_handle!(
    /// Identifies a document within the current host session.
    DocumentHandle
);

define_handle!(
    /// Identifies a view (editor pane) within the current host session.
    ViewHandle
);

define_handle!(
    /// Identifies a plugin-registered panel.
    PanelHandle
);

define_handle!(
    /// Identifies a plugin-registered command.
    CommandHandle
);

define_handle!(
    /// Identifies an active event subscription.
    SubscriptionHandle
);

define_handle!(
    /// Identifies a loaded plugin.
    PluginId
);

define_handle!(
    /// Identifies a floating window overlay.
    FloatHandle
);

define_handle!(
    /// Identifies a plugin render callback registered with the language host.
    RenderCallbackHandle
);

define_handle!(
    /// Identifies an assistant thread.
    ThreadHandle
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_round_trip() {
        let raw = NonZeroU64::new(42).unwrap();
        let h = DocumentHandle::from_raw(raw);
        assert_eq!(h.raw(), raw);
    }

    #[test]
    fn handle_equality() {
        let a = ViewHandle::from_raw(NonZeroU64::new(1).unwrap());
        let b = ViewHandle::from_raw(NonZeroU64::new(1).unwrap());
        let c = ViewHandle::from_raw(NonZeroU64::new(2).unwrap());
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn handle_display() {
        let h = PanelHandle::from_raw(NonZeroU64::new(7).unwrap());
        assert_eq!(format!("{h}"), "PanelHandle(7)");
    }

    #[test]
    fn handle_serde_round_trip() {
        let h = DocumentHandle::from_raw(NonZeroU64::new(99).unwrap());
        let bytes = super::super::codec::encode(&h).unwrap();
        let h2: DocumentHandle = super::super::codec::decode(&bytes).unwrap();
        assert_eq!(h, h2);
    }
}
