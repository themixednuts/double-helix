//! Capability traits defining the host boundary.
//!
//! `helix-plugin` defines these traits; host implementations (e.g., `helix-term`
//! for the terminal frontend) provide concrete implementations. The traits are
//! split by concern — there is no single monolithic `PluginHost`.
//!
//! Language-host adapters (Lua, future wasm/RPC) call through these traits
//! instead of reaching into editor internals directly.

use std::num::NonZeroU64;

use super::errors::ContractResult;
use super::events::EventKind;
use super::handles::*;
use super::metadata::{ApiMetadata, EventKindInfo};
use super::requests::*;
use super::snapshots::{
    AssistantContextSnapshot, AssistantEntrySnapshot, AssistantSnapshot, AssistantThreadSnapshot,
    DiagnosticSnapshot, DocumentSnapshot, FloatSnapshot, PanelSnapshot, SplitTreeSnapshot,
    TabGroupSnapshot, ThemeSnapshot, ViewSnapshot, WorkspaceDetailSnapshot, WorkspaceSnapshot,
};

// ---------------------------------------------------------------------------
// Query
// ---------------------------------------------------------------------------

/// Read-only access to editor/workspace state.
///
/// Safe to call from any plugin context (event handlers, commands, etc.).
/// Implementations must not mutate editor state.
pub trait PluginQueryHost {
    /// Return the host's API metadata for capability/version discovery.
    fn api_metadata(&self) -> ApiMetadata;

    // -- Workspace --

    /// The currently focused document, if any.
    fn focused_document(&self) -> Option<DocumentHandle>;

    /// The currently focused view, if any.
    fn focused_view(&self) -> Option<ViewHandle>;

    /// All open document handles.
    fn list_documents(&self) -> Vec<DocumentHandle>;

    /// All open view handles.
    fn list_views(&self) -> Vec<ViewHandle>;

    // -- Snapshots --

    /// Snapshot a document's metadata (path, language, selections, etc.).
    /// Does NOT include full text — use [`document_text`](Self::document_text).
    fn document_snapshot(&self, handle: DocumentHandle) -> ContractResult<DocumentSnapshot>;

    /// Snapshot a view's state (cursor, viewport, associated document).
    fn view_snapshot(&self, handle: ViewHandle) -> ContractResult<ViewSnapshot>;

    /// Snapshot the workspace (focused handles, document/view lists, mode).
    fn workspace_snapshot(&self) -> WorkspaceSnapshot;

    /// Snapshot the current theme's plugin-relevant colors.
    fn theme_snapshot(&self) -> ThemeSnapshot;

    /// Snapshot diagnostics for a document.
    fn diagnostics(&self, handle: DocumentHandle) -> ContractResult<DiagnosticSnapshot>;

    // -- Text content --

    /// Get the full text content of a document.
    fn document_text(&self, handle: DocumentHandle) -> ContractResult<String>;

    /// Get a single line (0-based) from a document.
    fn document_line(&self, handle: DocumentHandle, line: usize) -> ContractResult<String>;
}

// ---------------------------------------------------------------------------
// Mutation
// ---------------------------------------------------------------------------

/// Issue mutation requests against editor resources.
///
/// All mutations are request-based — the host validates and applies them.
/// Plugins never get direct mutable references to editor internals.
pub trait PluginMutationHost {
    /// Open a document by path and optionally focus it.
    fn open_document(&mut self, req: OpenDocumentRequest) -> ContractResult<DocumentHandle>;

    /// Apply text edits to a document.
    fn apply_edit(&mut self, req: ApplyEditRequest) -> ContractResult<()>;

    /// Set selections on a document (optionally in a specific view).
    fn set_selection(&mut self, req: SetSelectionRequest) -> ContractResult<()>;

    /// Save a document.
    fn save_document(&mut self, req: SaveDocumentRequest) -> ContractResult<()>;

    /// Focus a specific view.
    fn focus_view(&mut self, req: FocusViewRequest) -> ContractResult<()>;

    /// Set virtual text annotations on a document.
    fn set_annotations(&mut self, req: SetAnnotationsRequest) -> ContractResult<()>;

    /// Set the status line message.
    fn set_status(&mut self, req: SetStatusRequest) -> ContractResult<()>;

    /// Undo the last change in a document.
    fn undo(&mut self, req: UndoRequest) -> ContractResult<bool>;

    /// Redo the last undone change in a document.
    fn redo(&mut self, req: RedoRequest) -> ContractResult<bool>;

    /// Change the editor's mode (normal, insert, select).
    fn set_mode(&mut self, req: SetModeRequest) -> ContractResult<()>;

    /// Close a view.
    fn close_view(&mut self, req: CloseViewRequest) -> ContractResult<()>;
}

// ---------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------

/// Opaque token for an in-flight UI callback (prompt, confirm, picker).
///
/// The host returns this when a UI request is queued. The language-host adapter
/// uses it to correlate the response back to the originating plugin callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct UiCallbackToken(NonZeroU64);

impl UiCallbackToken {
    /// Wrap a raw non-zero callback identity value.
    pub const fn from_raw(id: NonZeroU64) -> Self {
        Self(id)
    }

    /// Extract the raw callback identity value.
    pub const fn raw(self) -> NonZeroU64 {
        self.0
    }
}

/// Frontend-dependent UI operations.
///
/// These are inherently async — the host queues the request and delivers the
/// result later via the callback token. The Lua facade wraps this as callback
/// functions.
pub trait PluginUiHost {
    /// Show a notification (fire-and-forget).
    fn notify(&mut self, req: NotifyRequest) -> ContractResult<()>;

    /// Show a text prompt. Returns a callback token for response correlation.
    fn prompt(&mut self, plugin: PluginId, req: PromptRequest) -> ContractResult<UiCallbackToken>;

    /// Show a yes/no confirmation. Returns a callback token.
    fn confirm(&mut self, plugin: PluginId, req: ConfirmRequest)
        -> ContractResult<UiCallbackToken>;

    /// Show a picker. Returns a callback token.
    fn picker(&mut self, plugin: PluginId, req: PickerRequest) -> ContractResult<UiCallbackToken>;
}

// ---------------------------------------------------------------------------
// Panels
// ---------------------------------------------------------------------------

/// Panel lifecycle management.
///
/// Panels are docked to an edge of the editor (left, right, bottom) and
/// persist across interactions. They can be toggled, focused, and resized.
pub trait PluginPanelHost {
    /// Register a new panel and get its handle.
    fn register_panel(
        &mut self,
        plugin: PluginId,
        reg: PanelRegistration,
    ) -> ContractResult<PanelHandle>;

    /// Update an existing panel's properties.
    fn update_panel(&mut self, plugin: PluginId, req: PanelUpdateRequest) -> ContractResult<()>;

    /// Close and remove a panel.
    fn close_panel(&mut self, plugin: PluginId, req: PanelCloseRequest) -> ContractResult<()>;

    /// Toggle a panel's visibility.
    fn toggle_panel(&mut self, plugin: PluginId, req: TogglePanelRequest) -> ContractResult<()>;

    /// Focus a panel (give it input focus).
    fn focus_panel(&mut self, plugin: PluginId, req: FocusPanelRequest) -> ContractResult<()>;

    /// Resize a panel.
    fn resize_panel(&mut self, plugin: PluginId, req: ResizePanelRequest) -> ContractResult<()>;

    /// List all registered panels.
    fn list_panels(&self) -> Vec<PanelSnapshot>;
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Command registration and invocation.
pub trait PluginCommandHost {
    /// Register a plugin command and get its handle.
    fn register_command(
        &mut self,
        plugin: PluginId,
        def: CommandDefinition,
    ) -> ContractResult<CommandHandle>;

    /// Update a plugin command's registration metadata.
    fn update_command(&mut self, plugin: PluginId, req: CommandUpdateRequest)
        -> ContractResult<()>;

    /// Remove a plugin command registration.
    fn remove_command(&mut self, plugin: PluginId, req: CommandRemoveRequest)
        -> ContractResult<()>;

    /// Run a command by name with arguments.
    fn run_command(&mut self, req: RunCommandRequest) -> ContractResult<()>;
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// Event subscription management.
pub trait PluginEventHost {
    /// Subscribe to an event kind. Returns a subscription handle for later
    /// unsubscription.
    fn subscribe(
        &mut self,
        plugin: PluginId,
        kind: EventKind,
    ) -> ContractResult<SubscriptionHandle>;

    /// Remove an event subscription.
    fn unsubscribe(&mut self, plugin: PluginId, handle: SubscriptionHandle) -> ContractResult<()>;

    /// Return the host's event catalog for discovery.
    fn event_catalog(&self) -> Vec<EventKindInfo>;
}

// ---------------------------------------------------------------------------
// Splits
// ---------------------------------------------------------------------------

/// Split/view topology management.
///
/// Plugins can create, navigate, resize, swap, and inspect splits.
pub trait PluginSplitHost {
    /// Split a view, creating a new pane. Returns the new view's handle.
    fn split_view(&mut self, req: SplitViewRequest) -> ContractResult<ViewHandle>;

    /// Navigate focus to an adjacent view. Returns the newly focused view, if any.
    fn focus_direction(&mut self, req: FocusDirectionRequest)
        -> ContractResult<Option<ViewHandle>>;

    /// Swap the focused view with an adjacent one.
    fn swap_split(&mut self, req: SwapSplitRequest) -> ContractResult<()>;

    /// Resize a split pane.
    fn resize_split(&mut self, req: ResizeSplitRequest) -> ContractResult<()>;

    /// Transpose the parent container's layout (horizontal ↔ vertical).
    fn transpose(&mut self, req: TransposeSplitRequest) -> ContractResult<()>;

    /// Snapshot the full split tree topology.
    fn split_tree(&self) -> SplitTreeSnapshot;
}

// ---------------------------------------------------------------------------
// Tabs
// ---------------------------------------------------------------------------

/// Tab group management within views.
///
/// Each view can host a group of tabs (documents). Tabs are a view-level
/// concept, not a tree-level one — each split can independently have tabs.
pub trait PluginTabHost {
    /// Open a document as a new tab in a view's tab group.
    fn open_tab(&mut self, req: OpenTabRequest) -> ContractResult<()>;

    /// Close a tab. If it's the last tab, the view closes too.
    fn close_tab(&mut self, req: CloseTabRequest) -> ContractResult<()>;

    /// Focus a specific tab by index.
    fn focus_tab(&mut self, req: FocusTabRequest) -> ContractResult<()>;

    /// Cycle to the next or previous tab.
    fn cycle_tab(&mut self, req: CycleTabRequest) -> ContractResult<()>;

    /// List tabs in a view's tab group.
    fn list_tabs(&self, view: Option<ViewHandle>) -> ContractResult<TabGroupSnapshot>;
}

// ---------------------------------------------------------------------------
// Floats
// ---------------------------------------------------------------------------

/// Floating window management.
///
/// Floats are persistent overlays that render above the editor area but below
/// modal layers (prompts, pickers). They have handles and can be updated or
/// closed by the plugin.
pub trait PluginFloatHost {
    /// Create a floating window. Returns its handle.
    fn create_float(
        &mut self,
        plugin: PluginId,
        req: CreateFloatRequest,
    ) -> ContractResult<FloatHandle>;

    /// Update a floating window's properties.
    fn update_float(&mut self, req: UpdateFloatRequest) -> ContractResult<()>;

    /// Close a floating window.
    fn close_float(&mut self, req: CloseFloatRequest) -> ContractResult<()>;

    /// List all floating windows.
    fn list_floats(&self) -> Vec<FloatSnapshot>;
}

// ---------------------------------------------------------------------------
// Assistant
// ---------------------------------------------------------------------------

/// Read-only access to the assistant/AI system.
///
/// Plugins can observe assistant state, read thread contents, and list context
/// items. Mutations (submit prompt, attach context, etc.) go through
/// [`PluginAssistantMutationHost`].
pub trait PluginAssistantQueryHost {
    /// Snapshot the assistant system as a whole (threads, active thread, ready state).
    fn assistant_snapshot(&self) -> AssistantSnapshot;

    /// Snapshot a specific thread by ID.
    fn thread_snapshot(&self, thread: ThreadHandle) -> ContractResult<AssistantThreadSnapshot>;

    /// List entries in a thread.
    fn thread_entries(&self, thread: ThreadHandle) -> ContractResult<Vec<AssistantEntrySnapshot>>;

    /// List context items attached to a thread.
    fn thread_context(&self, thread: ThreadHandle)
        -> ContractResult<Vec<AssistantContextSnapshot>>;
}

/// Mutation operations on the assistant/AI system.
///
/// These map to `assistant::Action` variants internally and follow the same
/// action → effect pattern as all other assistant state changes.
pub trait PluginAssistantMutationHost {
    /// Submit a prompt to a thread. If `thread` is `None`, targets the active thread.
    fn submit_prompt(&mut self, thread: Option<ThreadHandle>, text: String) -> ContractResult<()>;

    /// Cancel the active run on a thread. If `thread` is `None`, targets the active thread.
    fn cancel_thread(&mut self, thread: Option<ThreadHandle>) -> ContractResult<()>;
}

// ---------------------------------------------------------------------------
// Extended query
// ---------------------------------------------------------------------------

/// Extended workspace queries including UI topology.
pub trait PluginWorkspaceQueryHost {
    /// Full workspace snapshot with split tree, panels, floats, and focus state.
    fn workspace_detail(&self) -> WorkspaceDetailSnapshot;
}

/// Read-only facade surface used by language runtimes.
///
/// This groups query-like operations that are implemented by different local
/// bridge traits but are one synchronous RPC round-trip in a remote host.
pub trait PluginFacadeQueryHost:
    PluginQueryHost + PluginAssistantQueryHost + PluginWorkspaceQueryHost
{
    fn split_tree(&self) -> SplitTreeSnapshot;
    fn list_tabs(&self, view: Option<ViewHandle>) -> ContractResult<TabGroupSnapshot>;
}

/// Mutable facade surface used by language runtimes.
pub trait PluginFacadeMutationHost:
    PluginMutationHost + PluginSplitHost + PluginTabHost + PluginFloatHost + PluginAssistantMutationHost
{
}
