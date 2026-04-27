# Host-Agnostic Plugin API — Concrete Sketches

**Status:** Draft
**Date:** 2026-04-14
**Depends on:** `specs/host-agnostic-plugin-api.md`, `specs/host-agnostic-plugin-api-migration.md`

## Purpose

This document provides concrete Rust type/trait definitions and final-form Lua API examples for the host-agnostic plugin contract. It bridges the abstract spec and the Phase 2 implementation by showing exactly what the public types, capability traits, and Lua facade should look like.

Everything here targets `helix-plugin/src/contract/` — the canonical contract layer that all language hosts (Lua today, potentially others later) adapt to.

---

## Part 1: Rust Contract Types

### 1.1 Handles (`contract/handles.rs`)

Handles are opaque, `Copy`, serializable identity tokens. They do not carry mutable state.

```rust
use std::num::NonZeroU64;

/// Macro to define a handle type: opaque newtype over NonZeroU64.
macro_rules! define_handle {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
        #[repr(transparent)]
        pub struct $name(NonZeroU64);

        impl $name {
            pub fn from_raw(id: NonZeroU64) -> Self { Self(id) }
            pub fn raw(self) -> NonZeroU64 { self.0 }
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
```

**Conversion from internal types:** `helix-view`'s `DocumentId` and `ViewId` both use slotmap keys. The host adapter (in `helix-term` or a thin adapter module) converts these to/from `NonZeroU64` at the boundary. The contract layer never imports slotmap.

### 1.2 Snapshots (`contract/snapshots.rs`)

Snapshots are immutable, serializable data. They represent a point-in-time view of editor state.

```rust
use crate::contract::handles::{DocumentHandle, ViewHandle};

/// Immutable snapshot of a document's public state.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DocumentSnapshot {
    pub handle: DocumentHandle,
    pub path: Option<String>,
    pub language: Option<String>,
    pub is_modified: bool,
    pub line_count: usize,
    pub selections: Vec<SelectionRange>,
    pub mode: EditMode,
}

/// Immutable snapshot of a view's public state.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ViewSnapshot {
    pub handle: ViewHandle,
    pub document: DocumentHandle,
    pub cursor: Position,
    pub viewport: ViewportInfo,
}

/// Immutable snapshot of workspace-level state.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceSnapshot {
    pub focused_document: Option<DocumentHandle>,
    pub focused_view: Option<ViewHandle>,
    pub documents: Vec<DocumentHandle>,
    pub views: Vec<ViewHandle>,
    pub mode: EditMode,
}

/// Immutable snapshot of theme colors relevant to plugins.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ThemeSnapshot {
    pub name: String,
    pub bg: Option<Color>,
    pub fg: Option<Color>,
    pub selection: Option<Color>,
    pub cursor: Option<Color>,
    // Extensible: plugins can query named scopes via the host
}

/// Immutable snapshot of diagnostics for a document.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DiagnosticSnapshot {
    pub document: DocumentHandle,
    pub diagnostics: Vec<Diagnostic>,
}

// --- Supporting types ---

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct Position {
    pub line: usize,   // 0-based
    pub column: usize,  // 0-based, byte offset within line
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct SelectionRange {
    pub anchor: Position,
    pub head: Position,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum EditMode {
    Normal,
    Insert,
    Select,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct ViewportInfo {
    pub first_visible_line: usize,
    pub height: usize,
    pub width: usize,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Diagnostic {
    pub range: (Position, Position),
    pub message: String,
    pub severity: DiagnosticSeverity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Info,
    Hint,
}
```

### 1.3 Requests (`contract/requests.rs`)

Requests are the primary mutation mechanism. Each request is a named struct with explicit fields.

```rust
use crate::contract::handles::{DocumentHandle, ViewHandle, PanelHandle, PluginId};
use crate::contract::snapshots::Position;

// --- Document requests ---

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OpenDocumentRequest {
    pub path: String,
    /// If true, focus the newly opened document.
    pub focus: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ApplyEditRequest {
    pub document: DocumentHandle,
    pub edits: Vec<TextEdit>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TextEdit {
    pub start: Position,
    pub end: Position,
    pub new_text: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SetSelectionRequest {
    pub document: DocumentHandle,
    /// Optional: which view's selection to change. If None, changes the focused view's selection.
    pub view: Option<ViewHandle>,
    pub selections: Vec<crate::contract::snapshots::SelectionRange>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SaveDocumentRequest {
    pub document: DocumentHandle,
    /// If true, force save even without modifications.
    pub force: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SetAnnotationsRequest {
    pub document: DocumentHandle,
    pub plugin: PluginId,
    pub annotations: Vec<Annotation>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Annotation {
    pub position: Position,
    pub text: String,
    pub style: AnnotationStyle,
    /// If true, renders as a virtual line instead of inline.
    pub is_line: bool,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct AnnotationStyle {
    pub fg: Option<crate::contract::snapshots::Color>,
    pub bg: Option<crate::contract::snapshots::Color>,
}

// --- View requests ---

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FocusViewRequest {
    pub view: ViewHandle,
}

// --- UI requests ---

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PromptRequest {
    pub message: String,
    pub default: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConfirmRequest {
    pub message: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PickerRequest {
    pub items: Vec<String>,
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NotifyRequest {
    pub message: String,
    pub level: NotifyLevel,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub enum NotifyLevel {
    Info,
    Warn,
    Error,
}

// --- Panel requests ---

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PanelRegistration {
    pub title: String,
    pub side: PanelSide,
    pub width: Option<u16>,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub enum PanelSide {
    Left,
    Right,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PanelUpdateRequest {
    pub panel: PanelHandle,
    pub title: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PanelCloseRequest {
    pub panel: PanelHandle,
}

// --- Command requests ---

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CommandDefinition {
    pub name: String,
    pub doc: Option<String>,
    pub args: Option<Vec<String>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RunCommandRequest {
    pub name: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CommandUpdateRequest {
    pub command: CommandHandle,
    pub name: Option<String>,
    pub doc: Option<String>,
    pub args: Option<Vec<String>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CommandRemoveRequest {
    pub command: CommandHandle,
}
```

### 1.4 Events (`contract/events.rs`)

Events are structured, typed observations. The enum is the source of truth; Lua event names are aliases.

```rust
use crate::contract::handles::{DocumentHandle, ViewHandle};
use crate::contract::snapshots::{EditMode, Position};

/// All events a plugin can subscribe to.
///
/// Each variant is a structured event with its own payload type.
/// The Lua facade maps these to ergonomic event kind constants.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
}

/// Stable event kind identifiers. Used for subscription, not dynamic strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
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
}

impl PluginEvent {
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
        }
    }
}

// --- Event payloads ---

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HostReadyEvent {
    pub api_version: u32,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DocumentOpenedEvent {
    pub document: DocumentHandle,
    pub path: Option<String>,
    pub language: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DocumentChangedEvent {
    pub document: DocumentHandle,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DocumentPreSaveEvent {
    pub document: DocumentHandle,
    pub path: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DocumentSavedEvent {
    pub document: DocumentHandle,
    pub path: Option<String>,
    pub success: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DocumentClosedEvent {
    pub document: DocumentHandle,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SelectionChangedEvent {
    pub document: DocumentHandle,
    pub view: ViewHandle,
    pub primary_cursor: Position,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ModeChangedEvent {
    pub old: EditMode,
    pub new: EditMode,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ViewFocusedEvent {
    pub view: ViewHandle,
    pub document: DocumentHandle,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DiagnosticsUpdatedEvent {
    pub document: DocumentHandle,
    pub count: usize,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LspAttachedEvent {
    pub document: DocumentHandle,
    pub server_name: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct KeyPressedEvent {
    pub key: String,
    pub mode: EditMode,
}
```

### 1.5 Errors (`contract/errors.rs`)

```rust
/// Structured error type for all plugin API operations.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, thiserror::Error)]
pub enum PluginError {
    #[error("not found: {entity}")]
    NotFound { entity: String },

    #[error("stale handle: the referenced {entity} no longer exists")]
    StaleHandle { entity: String },

    #[error("invalid request: {reason}")]
    InvalidRequest { reason: String },

    #[error("permission denied: {reason}")]
    PermissionDenied { reason: String },

    #[error("unsupported capability: {capability}")]
    UnsupportedCapability { capability: String },

    #[error("busy: {reason}")]
    Busy { reason: String },

    #[error("internal host error: {message}")]
    InternalError { message: String },
}

/// Result type alias for plugin API operations.
pub type PluginResult<T> = Result<T, PluginError>;
```

### 1.6 Metadata (`contract/metadata.rs`)

```rust
/// API metadata for capability discovery and version negotiation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ApiMetadata {
    /// Current API version (monotonically increasing).
    pub version: u32,
    /// Minimum version a plugin must target to be compatible.
    pub min_compatible_version: u32,
    /// Which capability families this host supports.
    pub capabilities: Vec<Capability>,
    /// Catalog of subscribable event kinds.
    pub event_catalog: Vec<EventKindInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Capability {
    Query,
    Mutation,
    Ui,
    Panels,
    Commands,
    Events,
    Render,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EventKindInfo {
    pub kind: crate::contract::events::EventKind,
    pub description: String,
    pub since_version: u32,
}
```

---

## Part 2: Capability Traits

These traits define the host boundary. `helix-plugin` defines them; `helix-term` (or any future frontend) implements them.

### 2.1 Query (`contract/host.rs`)

```rust
use crate::contract::errors::PluginResult;
use crate::contract::handles::*;
use crate::contract::snapshots::*;
use crate::contract::metadata::ApiMetadata;

/// Read-only access to editor/workspace state. Safe to call from any context.
pub trait PluginQueryHost {
    fn api_metadata(&self) -> ApiMetadata;

    // Workspace
    fn focused_document(&self) -> Option<DocumentHandle>;
    fn focused_view(&self) -> Option<ViewHandle>;
    fn list_documents(&self) -> Vec<DocumentHandle>;
    fn list_views(&self) -> Vec<ViewHandle>;

    // Snapshots
    fn document_snapshot(&self, handle: DocumentHandle) -> PluginResult<DocumentSnapshot>;
    fn view_snapshot(&self, handle: ViewHandle) -> PluginResult<ViewSnapshot>;
    fn workspace_snapshot(&self) -> WorkspaceSnapshot;
    fn theme_snapshot(&self) -> ThemeSnapshot;
    fn diagnostics(&self, handle: DocumentHandle) -> PluginResult<DiagnosticSnapshot>;

    // Text content (separate from snapshot for efficiency — avoids copying full text)
    fn document_text(&self, handle: DocumentHandle) -> PluginResult<String>;
    fn document_line(&self, handle: DocumentHandle, line: usize) -> PluginResult<String>;
}
```

### 2.2 Mutation (`contract/host.rs`)

```rust
use crate::contract::requests::*;

/// Issue mutation requests against editor resources.
pub trait PluginMutationHost {
    fn open_document(&mut self, req: OpenDocumentRequest) -> PluginResult<DocumentHandle>;
    fn apply_edit(&mut self, req: ApplyEditRequest) -> PluginResult<()>;
    fn set_selection(&mut self, req: SetSelectionRequest) -> PluginResult<()>;
    fn save_document(&mut self, req: SaveDocumentRequest) -> PluginResult<()>;
    fn focus_view(&mut self, req: FocusViewRequest) -> PluginResult<()>;
    fn set_annotations(&mut self, req: SetAnnotationsRequest) -> PluginResult<()>;
    fn set_status(&mut self, message: String) -> PluginResult<()>;
}
```

### 2.3 UI (`contract/host.rs`)

```rust
/// Frontend-dependent UI operations. Capability-gated.
///
/// These are async in nature — the host queues the request and delivers
/// the result via a callback token. The Lua facade wraps this as coroutines
/// or callback functions.
pub trait PluginUiHost {
    fn notify(&mut self, req: NotifyRequest) -> PluginResult<()>;
    fn prompt(&mut self, plugin: PluginId, req: PromptRequest) -> PluginResult<UiCallbackToken>;
    fn confirm(&mut self, plugin: PluginId, req: ConfirmRequest) -> PluginResult<UiCallbackToken>;
    fn picker(&mut self, plugin: PluginId, req: PickerRequest) -> PluginResult<UiCallbackToken>;
}

/// Opaque token for an in-flight UI callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UiCallbackToken(pub std::num::NonZeroU64);
```

### 2.4 Panels (`contract/host.rs`)

```rust
/// Panel lifecycle management.
pub trait PluginPanelHost {
    fn register_panel(&mut self, plugin: PluginId, reg: PanelRegistration) -> PluginResult<PanelHandle>;
    fn update_panel(&mut self, req: PanelUpdateRequest) -> PluginResult<()>;
    fn close_panel(&mut self, req: PanelCloseRequest) -> PluginResult<()>;
}
```

### 2.5 Commands (`contract/host.rs`)

```rust
/// Command registration and invocation.
pub trait PluginCommandHost {
    fn register_command(&mut self, plugin: PluginId, def: CommandDefinition) -> PluginResult<CommandHandle>;
    fn update_command(&mut self, req: CommandUpdateRequest) -> PluginResult<()>;
    fn remove_command(&mut self, req: CommandRemoveRequest) -> PluginResult<()>;
    fn run_command(&mut self, req: RunCommandRequest) -> PluginResult<()>;
}
```

### 2.6 Events (`contract/host.rs`)

```rust
use crate::contract::events::EventKind;

/// Event subscription management.
pub trait PluginEventHost {
    fn subscribe(&mut self, plugin: PluginId, kind: EventKind) -> PluginResult<SubscriptionHandle>;
    fn unsubscribe(&mut self, handle: SubscriptionHandle) -> PluginResult<()>;
    fn event_catalog(&self) -> Vec<crate::contract::metadata::EventKindInfo>;
}
```

---

## Part 3: Contract Module Layout

Target directory structure inside `helix-plugin/src/`:

```
contract/
  mod.rs          -- re-exports
  handles.rs      -- opaque handle types
  snapshots.rs    -- immutable snapshot types
  requests.rs     -- mutation request types
  events.rs       -- event enum + payloads + EventKind
  errors.rs       -- PluginError + PluginResult
  metadata.rs     -- ApiMetadata, Capability, EventKindInfo
  host.rs         -- capability traits (PluginQueryHost, etc.)
```

`contract/mod.rs`:
```rust
pub mod handles;
pub mod snapshots;
pub mod requests;
pub mod events;
pub mod errors;
pub mod metadata;
pub mod host;

// Convenience re-exports
pub use errors::{PluginError, PluginResult};
pub use handles::*;
```

The existing `types.rs` stays for now — it holds the current runtime types (`UiHandler`, `UiCallbackId`, `PluginConfig`, etc.). The contract module is additive. During Phase 5 of the migration, the Lua facade switches from the old types to the contract types, and the old types can be retired.

---

## Part 4: Final Lua Facade Examples

These show how the Lua API should look once the facade is built over the contract. Each example maps to specific capability traits and contract types.

### 4.1 Module structure

```lua
-- All access goes through the top-level `helix` module
local helix = require('helix')

-- Sub-modules mirror the contract:
--   helix.workspace   -- WorkspaceSnapshot, focused handles
--   helix.documents   -- document queries and mutations
--   helix.views       -- view queries and mutations
--   helix.panels      -- panel registration and lifecycle
--   helix.commands    -- command registration and invocation
--   helix.ui          -- notifications, prompts, pickers
--   helix.events      -- typed event subscription
--   helix.log         -- structured logging
--   helix.host        -- API metadata and capabilities
```

### 4.2 Querying workspace state

```lua
local helix = require('helix')

-- Get the focused document handle (may be nil)
local doc = helix.workspace.focused_document()
if not doc then
  helix.log.warn("No focused document")
  return
end

-- Get a snapshot — immutable table, cheap to pass around
local snap = helix.documents.snapshot(doc)
helix.log.info(string.format(
  "Editing %s (%s, %d lines, %s)",
  snap.path or "[scratch]",
  snap.language or "plain",
  snap.line_count,
  snap.is_modified and "modified" or "clean"
))
```

### 4.3 Applying edits to a specific document

```lua
local helix = require('helix')

-- Plugins can target ANY document, not just the focused one
local doc = helix.workspace.focused_document()

helix.documents.apply_edit {
  document = doc,
  edits = {
    {
      start  = { line = 0, column = 0 },
      finish = { line = 0, column = 0 },
      text   = "// Auto-generated header\n",
    },
  },
}
```

### 4.4 Working with views

```lua
local helix = require('helix')

-- List all views, find which document each shows
for _, view in ipairs(helix.views.list()) do
  local snap = helix.views.snapshot(view)
  local doc_snap = helix.documents.snapshot(snap.document)
  helix.log.info(string.format(
    "View %s → %s",
    tostring(view),
    doc_snap.path or "[scratch]"
  ))
end

-- Focus a specific view
helix.views.focus { view = some_view_handle }
```

### 4.5 Event subscription with typed event kinds

```lua
local helix = require('helix')

-- Subscribe using stable event kind constants, not free-form strings
helix.events.subscribe(helix.events.kind.DocumentOpened, function(event)
  helix.log.info(string.format("Opened: %s (%s)",
    event.path or "[scratch]",
    event.language or "unknown"
  ))
end)

helix.events.subscribe(helix.events.kind.DocumentChanged, function(event)
  -- event.document is a DocumentHandle — use it for targeted queries
  local snap = helix.documents.snapshot(event.document)
  if snap.line_count > 10000 then
    helix.ui.notify { message = "Large file!", level = "warn" }
  end
end)

helix.events.subscribe(helix.events.kind.ModeChanged, function(event)
  helix.log.debug(string.format("Mode: %s → %s", event.old, event.new))
end)
```

### 4.6 UI interactions

```lua
local helix = require('helix')

-- Simple notification
helix.ui.notify { message = "Plugin loaded", level = "info" }

-- Prompt with callback
helix.ui.prompt {
  message = "Search term:",
  default = "",
  callback = function(result)
    if result then
      helix.log.info("User entered: " .. result)
    end
  end,
}

-- Confirm dialog
helix.ui.confirm {
  message = "Delete all trailing whitespace?",
  callback = function(yes)
    if yes then
      -- apply edits...
    end
  end,
}

-- Picker
helix.ui.picker {
  items = { "Option A", "Option B", "Option C" },
  prompt = "Choose one:",
  callback = function(selection)
    if selection then
      helix.log.info("Picked: " .. selection)
    end
  end,
}
```

### 4.7 Panel registration

```lua
local helix = require('helix')

local panel = helix.panels.register {
  title = "My Plugin",
  side = "right",
  width = 40,
  on_render = function(surface, area, theme)
    -- surface provides stateless drawing primitives
    surface:header(area, "My Plugin Panel", theme)
    surface:set_string(area.x, area.y + 1, "Hello from plugin!", theme.default)
  end,
  on_event = function(event)
    -- handle panel-specific key events
  end,
}

-- Later: update or close
helix.panels.update { panel = panel, title = "Updated Title" }
helix.panels.close { panel = panel }
```

### 4.8 Command registration

```lua
local helix = require('helix')

helix.commands.register {
  name = "trim-whitespace",
  doc = "Remove trailing whitespace from the current document",
  handler = function(args)
    local doc = helix.workspace.focused_document()
    if not doc then return end

    local snap = helix.documents.snapshot(doc)
    local edits = {}
    for i = 0, snap.line_count - 1 do
      local line = helix.documents.line(doc, i)
      local trimmed = line:gsub("%s+$", "")
      if #trimmed ~= #line then
        table.insert(edits, {
          start  = { line = i, column = #trimmed },
          finish = { line = i, column = #line },
          text   = "",
        })
      end
    end

    if #edits > 0 then
      helix.documents.apply_edit { document = doc, edits = edits }
      helix.ui.notify {
        message = string.format("Trimmed %d lines", #edits),
        level = "info",
      }
    end
  end,
}
```

### 4.9 Host capability discovery

```lua
local helix = require('helix')

local meta = helix.host.api_metadata()
helix.log.info(string.format("API v%d (min compat: v%d)", meta.version, meta.min_compatible_version))

-- Check capabilities before using optional features
if meta:has_capability("panels") then
  -- register panels
end

-- Inspect event catalog
for _, info in ipairs(meta.event_catalog) do
  helix.log.debug(string.format("Event: %s (since v%d) — %s",
    info.kind, info.since_version, info.description))
end
```

### 4.10 Selections and diagnostics

```lua
local helix = require('helix')

-- Read selections from a specific document
local doc = helix.workspace.focused_document()
local snap = helix.documents.snapshot(doc)

for i, sel in ipairs(snap.selections) do
  helix.log.info(string.format(
    "Selection %d: (%d,%d) → (%d,%d)",
    i, sel.anchor.line, sel.anchor.column, sel.head.line, sel.head.column
  ))
end

-- Set selections explicitly
helix.documents.set_selection {
  document = doc,
  selections = {
    { anchor = { line = 0, column = 0 }, head = { line = 0, column = 5 } },
  },
}

-- Read diagnostics
local diags = helix.documents.diagnostics(doc)
for _, d in ipairs(diags.diagnostics) do
  helix.log.info(string.format("[%s] L%d: %s", d.severity, d.range[1].line, d.message))
end
```

---

## Part 5: Mapping Current → New (Concrete)

| Current Code | New Contract Type | New Lua API |
|---|---|---|
| `PluginDocumentHandle(u64)` | `DocumentHandle(NonZeroU64)` | opaque userdata |
| `PluginViewHandle(u64)` | `ViewHandle(NonZeroU64)` | opaque userdata |
| `UiCallbackId(NonZeroU64)` | `UiCallbackToken(NonZeroU64)` | internal, not exposed |
| `PromptRequest { message, default, plugin_name, callback_id }` | `PromptRequest { message, default }` | `helix.ui.prompt { message, default, callback }` |
| `ConfirmRequest { message, plugin_name, callback_id }` | `ConfirmRequest { message }` | `helix.ui.confirm { message, callback }` |
| `PickerRequest { items, prompt, plugin_name, callback_id }` | `PickerRequest { items, prompt }` | `helix.ui.picker { items, prompt, callback }` |
| `PanelRegistration { plugin_name, panel_id, title, side, width, render_callback_id, event_callback_id }` | `PanelRegistration { title, side, width }` | `helix.panels.register { title, side, width, on_render, on_event }` |
| `UiHandler` trait | `PluginUiHost` + `PluginPanelHost` | split by concern |
| `EditorCommandRegistry` trait | `PluginCommandHost` | `helix.commands.run { name, args }` |
| `DrawSurface` trait | stays as render capability (RenderContext) | `surface:*` in panel callbacks |
| `EventType` string enum | `EventKind` typed enum | `helix.events.kind.*` |
| `PluginEvent` enum | `PluginEvent` enum (restructured payloads) | callback receives structured table |
| thread-local `CURRENT_EDITOR` | `PluginQueryHost` / `PluginMutationHost` | transparent — Lua calls route through traits |

---

## Part 6: Key Design Decisions for Implementation

### Plugin name / callback ID removed from request types

Current requests carry `plugin_name` and `callback_id` because the Lua runtime needs to route responses. In the new design, these are runtime concerns — the Lua adapter attaches them when bridging from `helix.ui.prompt { ..., callback = fn }` to the host trait call `prompt(plugin_id, req)`. The contract request type stays clean.

### Handle conversion at the boundary

`DocumentHandle` ↔ `DocumentId` conversion lives in a thin adapter (likely in `helix-plugin` or a `bridge` module). The contract never imports slotmap. Example:

```rust
impl From<DocumentId> for DocumentHandle {
    fn from(id: DocumentId) -> Self {
        // slotmap keys have a .data() -> KeyData, which has idx + version
        let raw = id.data().as_ffi();
        DocumentHandle::from_raw(NonZeroU64::new(raw).expect("slotmap keys are never zero"))
    }
}
```

### Snapshot vs full text

`DocumentSnapshot` intentionally does NOT include the full text content. Full text is available via `PluginQueryHost::document_text()`. This keeps snapshots lightweight for common operations (checking metadata, line count, language) while still allowing full text access when needed.

### Callbacks vs futures in the Lua facade

UI operations (prompt, confirm, picker) are inherently async. The Lua facade uses callbacks today, and that stays. The contract trait uses `UiCallbackToken` so the host can match responses to requests. A future coroutine-based Lua API could wrap this, but callbacks remain the primary model for now.

---

## Next Step: Phase 2 Implementation

With these concrete sketches in hand, the implementation plan for Phase 2 is:

1. Create `helix-plugin/src/contract/` module with the files listed in Part 3
2. Define all types from Parts 1.1–1.6 (handles, snapshots, requests, events, errors, metadata)
3. Define all capability traits from Part 2 (host.rs)
4. Add `serde` derive support (for future transport compatibility)
5. Write unit tests for handle construction, event kind mapping, error display
6. Do NOT yet wire anything to the existing Lua runtime — that's Phase 5

The existing `types.rs` and Lua modules continue working unchanged. The contract module is purely additive.
