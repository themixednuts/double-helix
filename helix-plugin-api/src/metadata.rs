//! API metadata and capability discovery for the host-agnostic plugin contract.
//!
//! Plugins can query the host for its capabilities, supported API version,
//! and event catalog. This allows forward-compatible plugins and lets future
//! non-Lua hosts advertise their support matrix.

use serde::{Deserialize, Serialize};

use super::events::EventKind;

/// Exact version of the unreleased plugin contract.
pub const API_VERSION: u32 = 1;

/// Host capability families.
///
/// A host advertises which families it supports. Plugins can check before
/// calling capability-gated APIs (e.g., panels may not exist in a headless host).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Capability {
    /// Read-only queries (always present in a conforming host).
    Query,
    /// Document/view mutations.
    Mutation,
    /// UI dialogs (prompt, confirm, picker, notify).
    Ui,
    /// Side panels.
    Panels,
    /// Command registration and invocation.
    Commands,
    /// Owned declarative keymap contributions.
    Keymaps,
    /// Event subscription.
    Events,
    /// Split/view topology management.
    Splits,
    /// Per-view tab groups.
    Tabs,
    /// Floating window overlays.
    Floats,
    /// Cancellable host-owned asynchronous operations.
    Tasks,
    /// Immutable syntax snapshots and background queries.
    Syntax,
    /// Language-server discovery and custom requests.
    Lsp,
    /// Theme snapshots and asynchronous activation.
    Themes,
    /// Assistant thread queries and mutations.
    Assistant,
}

/// Description of a single event kind in the host's catalog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventKindInfo {
    pub kind: EventKind,
    pub description: String,
}

/// Full API metadata returned by the host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiMetadata {
    /// Current API version of this host.
    pub version: u32,
    /// Supported capability families.
    pub capabilities: Vec<Capability>,
    /// Catalog of subscribable event kinds.
    pub event_catalog: Vec<EventKindInfo>,
}

impl ApiMetadata {
    #[must_use]
    pub fn from_capabilities(capabilities: impl IntoIterator<Item = Capability>) -> Self {
        let requested = capabilities
            .into_iter()
            .collect::<std::collections::HashSet<_>>();
        let capabilities = Capability::ALL
            .iter()
            .copied()
            .filter(|capability| requested.contains(capability))
            .collect::<Vec<_>>();
        let event_catalog = capabilities
            .contains(&Capability::Events)
            .then(default_event_catalog)
            .unwrap_or_default();
        Self {
            version: API_VERSION,
            capabilities,
            event_catalog,
        }
    }

    /// Check whether the host supports a given capability.
    pub fn has_capability(&self, cap: Capability) -> bool {
        self.capabilities.contains(&cap)
    }
}

impl Capability {
    pub const ALL: &[Capability] = &[
        Capability::Query,
        Capability::Mutation,
        Capability::Ui,
        Capability::Panels,
        Capability::Commands,
        Capability::Keymaps,
        Capability::Events,
        Capability::Splits,
        Capability::Tabs,
        Capability::Floats,
        Capability::Tasks,
        Capability::Syntax,
        Capability::Lsp,
        Capability::Themes,
        Capability::Assistant,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Capability::Query => "query",
            Capability::Mutation => "mutation",
            Capability::Ui => "ui",
            Capability::Panels => "panels",
            Capability::Commands => "commands",
            Capability::Keymaps => "keymaps",
            Capability::Events => "events",
            Capability::Splits => "splits",
            Capability::Tabs => "tabs",
            Capability::Floats => "floats",
            Capability::Tasks => "tasks",
            Capability::Syntax => "syntax",
            Capability::Lsp => "lsp",
            Capability::Themes => "themes",
            Capability::Assistant => "assistant",
        }
    }
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Capability {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Capability::ALL
            .iter()
            .copied()
            .find(|cap| cap.as_str() == value)
            .ok_or_else(|| format!("unknown capability: {value}"))
    }
}

impl Default for ApiMetadata {
    fn default() -> Self {
        Self::from_capabilities(Capability::ALL.iter().copied())
    }
}

/// Build the default event catalog from all known event kinds.
fn default_event_catalog() -> Vec<EventKindInfo> {
    EventKind::SUPPORTED
        .iter()
        .map(|&kind| EventKindInfo {
            kind,
            description: default_event_description(kind).into(),
        })
        .collect()
}

fn default_event_description(kind: EventKind) -> &'static str {
    match kind {
        EventKind::HostReady => "Fired when the host is ready to accept plugin calls",
        EventKind::DocumentOpened => "Fired when a document is opened",
        EventKind::DocumentChanged => "Fired when a document's content changes",
        EventKind::DocumentPreSave => "Fired before a document is saved",
        EventKind::DocumentSaved => "Fired after a document is saved",
        EventKind::DocumentClosed => "Fired when a document is closed",
        EventKind::SelectionChanged => "Fired when the selection changes",
        EventKind::ModeChanged => "Fired when the editing mode changes",
        EventKind::ViewFocused => "Fired when a view gains focus",
        EventKind::DiagnosticsUpdated => "Fired when diagnostics are updated",
        EventKind::LspAttached => "Fired when a language server attaches to a document",
        EventKind::KeyPressed => "Fired when a key is pressed",
        EventKind::SplitCreated => "Fired when a new split/view is created",
        EventKind::SplitClosed => "Fired when a split/view is closed",
        EventKind::TabOpened => "Fired when a new tab is opened in a view",
        EventKind::TabClosed => "Fired when a tab is closed in a view",
        EventKind::TabFocused => "Fired when a different tab is focused in a view",
        EventKind::FloatCreated => "Fired when a floating window is created",
        EventKind::FloatClosed => "Fired when a floating window is closed",
        EventKind::PanelToggled => "Fired when a panel is shown or hidden",
        EventKind::AssistantThreadCreated => "Fired when an assistant thread is created or loaded",
        EventKind::AssistantThreadClosed => "Fired when an assistant thread is closed",
        EventKind::AssistantRunStarted => "Fired when an assistant run begins",
        EventKind::AssistantRunCompleted => "Fired when an assistant run finishes",
        EventKind::AssistantMessageReceived => "Fired when a new assistant entry is appended",
        EventKind::AssistantContextChanged => {
            "Fired when assistant context is attached or detached"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_metadata_has_all_capabilities() {
        let meta = ApiMetadata::default();
        assert!(meta.has_capability(Capability::Query));
        assert!(meta.has_capability(Capability::Panels));
        assert!(meta.has_capability(Capability::Splits));
        assert!(meta.has_capability(Capability::Tabs));
        assert!(meta.has_capability(Capability::Floats));
    }

    #[test]
    fn default_catalog_contains_only_supported_events() {
        let meta = ApiMetadata::default();
        assert_eq!(meta.event_catalog.len(), EventKind::SUPPORTED.len());
        assert!(meta
            .event_catalog
            .iter()
            .all(|event| event.kind.is_supported()));
        assert!(!meta
            .event_catalog
            .iter()
            .any(|event| event.kind == EventKind::DocumentPreSave));
    }

    #[test]
    fn metadata_serde_round_trip() {
        let meta = ApiMetadata::default();
        let bytes = super::super::codec::encode(&meta).unwrap();
        let meta2: ApiMetadata = super::super::codec::decode(&bytes).unwrap();
        assert_eq!(meta2.version, 1);
        assert_eq!(meta2.capabilities.len(), meta.capabilities.len());
    }
}
