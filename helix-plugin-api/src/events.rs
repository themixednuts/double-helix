//! Typed event definitions for the host-agnostic plugin contract.
//!
//! Events are structured observations. The [`PluginEvent`] enum is the Rust
//! source of truth; the Lua facade maps [`EventKind`] constants to ergonomic
//! subscription names. No free-form string registration at the contract layer.

use serde::{Deserialize, Serialize};

use super::handles::{DocumentHandle, FloatHandle, PanelHandle, ThreadHandle, ViewHandle};
use super::snapshots::{EditMode, Position};

// ---------------------------------------------------------------------------
// Event kind — stable identifiers for subscription
// ---------------------------------------------------------------------------

/// Stable event kind identifiers used for subscription.
///
/// Plugins subscribe to an `EventKind`; the host dispatches the matching
/// [`PluginEvent`] variant when it fires. This replaces string-based
/// registration with a typed catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    HostReady,
    DocumentOpened,
    DocumentChanged,
    DocumentPreSave,
    DocumentSaved,
    DocumentClosed,
    SelectionChanged,
    ModeChanged,
    ViewFocused,
    DiagnosticsUpdated,
    LspAttached,
    KeyPressed,
    SplitCreated,
    SplitClosed,
    TabOpened,
    TabClosed,
    TabFocused,
    FloatCreated,
    FloatClosed,
    PanelToggled,
    AssistantThreadCreated,
    AssistantThreadClosed,
    AssistantRunStarted,
    AssistantRunCompleted,
    AssistantMessageReceived,
    AssistantContextChanged,
}

impl EventKind {
    /// All event kinds, for catalog/metadata enumeration.
    pub const ALL: &[EventKind] = &[
        Self::HostReady,
        Self::DocumentOpened,
        Self::DocumentChanged,
        Self::DocumentPreSave,
        Self::DocumentSaved,
        Self::DocumentClosed,
        Self::SelectionChanged,
        Self::ModeChanged,
        Self::ViewFocused,
        Self::DiagnosticsUpdated,
        Self::LspAttached,
        Self::KeyPressed,
        Self::SplitCreated,
        Self::SplitClosed,
        Self::TabOpened,
        Self::TabClosed,
        Self::TabFocused,
        Self::FloatCreated,
        Self::FloatClosed,
        Self::PanelToggled,
        Self::AssistantThreadCreated,
        Self::AssistantThreadClosed,
        Self::AssistantRunStarted,
        Self::AssistantRunCompleted,
        Self::AssistantMessageReceived,
        Self::AssistantContextChanged,
    ];

    /// Events with complete editor emitters in the current host.
    pub const SUPPORTED: &[EventKind] = &[
        Self::HostReady,
        Self::DocumentOpened,
        Self::DocumentChanged,
        Self::DocumentSaved,
        Self::DocumentClosed,
        Self::SelectionChanged,
        Self::ModeChanged,
        Self::ViewFocused,
        Self::DiagnosticsUpdated,
        Self::KeyPressed,
        Self::AssistantThreadCreated,
        Self::AssistantThreadClosed,
        Self::AssistantRunStarted,
        Self::AssistantRunCompleted,
        Self::AssistantMessageReceived,
        Self::AssistantContextChanged,
    ];

    pub fn is_supported(self) -> bool {
        Self::SUPPORTED.contains(&self)
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|kind| kind.as_str() == id)
    }

    /// Human-readable name for this event kind.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HostReady => "host_ready",
            Self::DocumentOpened => "document_opened",
            Self::DocumentChanged => "document_changed",
            Self::DocumentPreSave => "document_pre_save",
            Self::DocumentSaved => "document_saved",
            Self::DocumentClosed => "document_closed",
            Self::SelectionChanged => "selection_changed",
            Self::ModeChanged => "mode_changed",
            Self::ViewFocused => "view_focused",
            Self::DiagnosticsUpdated => "diagnostics_updated",
            Self::LspAttached => "lsp_attached",
            Self::KeyPressed => "key_pressed",
            Self::SplitCreated => "split_created",
            Self::SplitClosed => "split_closed",
            Self::TabOpened => "tab_opened",
            Self::TabClosed => "tab_closed",
            Self::TabFocused => "tab_focused",
            Self::FloatCreated => "float_created",
            Self::FloatClosed => "float_closed",
            Self::PanelToggled => "panel_toggled",
            Self::AssistantThreadCreated => "assistant_thread_created",
            Self::AssistantThreadClosed => "assistant_thread_closed",
            Self::AssistantRunStarted => "assistant_run_started",
            Self::AssistantRunCompleted => "assistant_run_completed",
            Self::AssistantMessageReceived => "assistant_message_received",
            Self::AssistantContextChanged => "assistant_context_changed",
        }
    }
}

impl std::fmt::Display for EventKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod event_kind_tests {
    use super::EventKind;

    #[test]
    fn stable_id_round_trips_through_json() {
        let encoded = serde_json::to_string(&EventKind::DocumentOpened).unwrap();
        assert_eq!(encoded, "\"document_opened\"");
        assert_eq!(
            serde_json::from_str::<EventKind>(&encoded).unwrap(),
            EventKind::DocumentOpened
        );
        assert_eq!(
            EventKind::from_id("document_opened"),
            Some(EventKind::DocumentOpened)
        );
        assert_eq!(EventKind::from_id("DocumentOpened"), None);
    }
}

// ---------------------------------------------------------------------------
// Event payloads
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostReadyEvent {
    pub api_version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentOpenedEvent {
    pub document: DocumentHandle,
    pub path: Option<String>,
    pub language: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentChangedEvent {
    pub document: DocumentHandle,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentPreSaveEvent {
    pub document: DocumentHandle,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentSavedEvent {
    pub document: DocumentHandle,
    pub path: Option<String>,
    pub success: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentClosedEvent {
    pub document: DocumentHandle,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectionChangedEvent {
    pub document: DocumentHandle,
    pub view: ViewHandle,
    pub primary_cursor: Position,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModeChangedEvent {
    pub old: EditMode,
    pub new: EditMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewFocusedEvent {
    pub view: ViewHandle,
    pub document: DocumentHandle,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticsUpdatedEvent {
    pub document: DocumentHandle,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspAttachedEvent {
    pub document: DocumentHandle,
    pub server_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyPressedEvent {
    pub key: String,
    pub mode: EditMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SplitCreatedEvent {
    pub new_view: ViewHandle,
    pub source_view: ViewHandle,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SplitClosedEvent {
    pub view: ViewHandle,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabOpenedEvent {
    pub view: ViewHandle,
    pub document: DocumentHandle,
    pub index: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabClosedEvent {
    pub view: ViewHandle,
    pub document: DocumentHandle,
    pub index: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabFocusedEvent {
    pub view: ViewHandle,
    pub document: DocumentHandle,
    pub index: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FloatCreatedEvent {
    pub float: FloatHandle,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FloatClosedEvent {
    pub float: FloatHandle,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelToggledEvent {
    pub panel: PanelHandle,
    pub visible: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantThreadCreatedEvent {
    pub thread: ThreadHandle,
    pub title: Option<String>,
    pub scope_cwd: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantThreadClosedEvent {
    pub thread: ThreadHandle,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantRunStartedEvent {
    pub thread: ThreadHandle,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantRunCompletedEvent {
    pub thread: ThreadHandle,
    pub success: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantMessageReceivedEvent {
    pub thread: ThreadHandle,
    pub entry_id: u64,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantContextChangedEvent {
    pub thread: ThreadHandle,
    pub attached: bool,
    pub context_kind: String,
}

// ---------------------------------------------------------------------------
// Top-level event enum
// ---------------------------------------------------------------------------

/// All events a plugin can observe.
///
/// Each variant carries its own structured payload. The host constructs these
/// from internal state and dispatches them to subscribed plugins.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PluginEvent {
    HostReady(HostReadyEvent),
    DocumentOpened(DocumentOpenedEvent),
    DocumentChanged(DocumentChangedEvent),
    DocumentPreSave(DocumentPreSaveEvent),
    DocumentSaved(DocumentSavedEvent),
    DocumentClosed(DocumentClosedEvent),
    SelectionChanged(SelectionChangedEvent),
    ModeChanged(ModeChangedEvent),
    ViewFocused(ViewFocusedEvent),
    DiagnosticsUpdated(DiagnosticsUpdatedEvent),
    LspAttached(LspAttachedEvent),
    KeyPressed(KeyPressedEvent),
    SplitCreated(SplitCreatedEvent),
    SplitClosed(SplitClosedEvent),
    TabOpened(TabOpenedEvent),
    TabClosed(TabClosedEvent),
    TabFocused(TabFocusedEvent),
    FloatCreated(FloatCreatedEvent),
    FloatClosed(FloatClosedEvent),
    PanelToggled(PanelToggledEvent),
    AssistantThreadCreated(AssistantThreadCreatedEvent),
    AssistantThreadClosed(AssistantThreadClosedEvent),
    AssistantRunStarted(AssistantRunStartedEvent),
    AssistantRunCompleted(AssistantRunCompletedEvent),
    AssistantMessageReceived(AssistantMessageReceivedEvent),
    AssistantContextChanged(AssistantContextChangedEvent),
}

impl PluginEvent {
    /// Which event kind this event belongs to.
    pub fn kind(&self) -> EventKind {
        match self {
            Self::HostReady(_) => EventKind::HostReady,
            Self::DocumentOpened(_) => EventKind::DocumentOpened,
            Self::DocumentChanged(_) => EventKind::DocumentChanged,
            Self::DocumentPreSave(_) => EventKind::DocumentPreSave,
            Self::DocumentSaved(_) => EventKind::DocumentSaved,
            Self::DocumentClosed(_) => EventKind::DocumentClosed,
            Self::SelectionChanged(_) => EventKind::SelectionChanged,
            Self::ModeChanged(_) => EventKind::ModeChanged,
            Self::ViewFocused(_) => EventKind::ViewFocused,
            Self::DiagnosticsUpdated(_) => EventKind::DiagnosticsUpdated,
            Self::LspAttached(_) => EventKind::LspAttached,
            Self::KeyPressed(_) => EventKind::KeyPressed,
            Self::SplitCreated(_) => EventKind::SplitCreated,
            Self::SplitClosed(_) => EventKind::SplitClosed,
            Self::TabOpened(_) => EventKind::TabOpened,
            Self::TabClosed(_) => EventKind::TabClosed,
            Self::TabFocused(_) => EventKind::TabFocused,
            Self::FloatCreated(_) => EventKind::FloatCreated,
            Self::FloatClosed(_) => EventKind::FloatClosed,
            Self::PanelToggled(_) => EventKind::PanelToggled,
            Self::AssistantThreadCreated(_) => EventKind::AssistantThreadCreated,
            Self::AssistantThreadClosed(_) => EventKind::AssistantThreadClosed,
            Self::AssistantRunStarted(_) => EventKind::AssistantRunStarted,
            Self::AssistantRunCompleted(_) => EventKind::AssistantRunCompleted,
            Self::AssistantMessageReceived(_) => EventKind::AssistantMessageReceived,
            Self::AssistantContextChanged(_) => EventKind::AssistantContextChanged,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;

    fn doc(id: u64) -> DocumentHandle {
        DocumentHandle::from_raw(NonZeroU64::new(id).unwrap())
    }

    #[test]
    fn event_kind_round_trip() {
        let event = PluginEvent::DocumentOpened(DocumentOpenedEvent {
            document: doc(1),
            path: Some("/tmp/test.rs".into()),
            language: Some("rust".into()),
        });
        assert_eq!(event.kind(), EventKind::DocumentOpened);
    }

    #[test]
    fn event_kind_display() {
        assert_eq!(EventKind::DocumentChanged.to_string(), "document_changed");
        assert_eq!(EventKind::HostReady.to_string(), "host_ready");
    }

    #[test]
    fn event_kind_all_is_exhaustive() {
        // Ensure ALL covers every variant by checking length matches the
        // number of match arms in as_str.
        assert_eq!(EventKind::ALL.len(), 26);
    }

    #[test]
    fn event_serde_round_trip() {
        let event = PluginEvent::ModeChanged(ModeChangedEvent {
            old: EditMode::Normal,
            new: EditMode::Insert,
        });
        let bytes = super::super::codec::encode(&event).unwrap();
        let event2: PluginEvent = super::super::codec::decode(&bytes).unwrap();
        assert_eq!(event2.kind(), EventKind::ModeChanged);
    }
}
