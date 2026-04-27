//! Mutation request types for the host-agnostic plugin contract.
//!
//! Requests are the primary mutation mechanism. Each request is a named struct
//! with explicit fields — no positional protocols, no stringly-typed bags.
//! All requests are serializable for future transport compatibility.
//!
//! Runtime concerns like plugin names are NOT part of these types. Callback
//! identities are represented as opaque typed handles when a request needs
//! host-driven rendering.

use serde::{Deserialize, Serialize};

use super::handles::{
    CommandHandle, DocumentHandle, FloatHandle, PanelHandle, PluginId, RenderCallbackHandle,
    ViewHandle,
};
use super::snapshots::{Color, Position, SelectionRange};

fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Document requests
// ---------------------------------------------------------------------------

/// Request to open a document by path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenDocumentRequest {
    pub path: String,
    /// If true, focus the newly opened document.
    #[serde(default)]
    pub focus: bool,
}

/// Request to apply text edits to a document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyEditRequest {
    pub document: DocumentHandle,
    pub edits: Vec<TextEdit>,
}

/// A single text edit: replace the range `[start, end)` with `new_text`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextEdit {
    pub start: Position,
    pub end: Position,
    pub new_text: String,
}

/// Request to set selections on a document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetSelectionRequest {
    pub document: DocumentHandle,
    /// Which view's selection to change. If `None`, targets a visible view
    /// showing `document`, preferring the focused view when it matches.
    pub view: Option<ViewHandle>,
    pub selections: Vec<SelectionRange>,
}

/// Request to save a document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaveDocumentRequest {
    pub document: DocumentHandle,
    /// If true, force save even without modifications.
    #[serde(default)]
    pub force: bool,
}

/// Request to set virtual text annotations on a document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetAnnotationsRequest {
    pub document: DocumentHandle,
    pub plugin: PluginId,
    pub annotations: Vec<Annotation>,
}

/// A virtual text annotation attached to a document position.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Annotation {
    pub position: Position,
    pub text: String,
    #[serde(default)]
    pub style: AnnotationStyle,
    /// If true, renders as a virtual line instead of inline.
    #[serde(default)]
    pub is_line: bool,
}

/// Style for an annotation's text.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct AnnotationStyle {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
}

// ---------------------------------------------------------------------------
// Document editing requests
// ---------------------------------------------------------------------------

/// Request to undo the last change in a document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoRequest {
    pub document: DocumentHandle,
}

/// Request to redo the last undone change in a document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedoRequest {
    pub document: DocumentHandle,
}

// ---------------------------------------------------------------------------
// View requests
// ---------------------------------------------------------------------------

/// Request to focus a specific view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FocusViewRequest {
    pub view: ViewHandle,
}

/// Request to close a specific view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloseViewRequest {
    pub view: ViewHandle,
}

// ---------------------------------------------------------------------------
// Editor mode
// ---------------------------------------------------------------------------

/// Request to change the editor's mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetModeRequest {
    pub mode: super::snapshots::EditMode,
}

// ---------------------------------------------------------------------------
// UI requests
// ---------------------------------------------------------------------------

/// Request to show a text prompt to the user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptRequest {
    pub message: String,
    pub default: Option<String>,
}

/// Request to show a yes/no confirmation dialog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfirmRequest {
    pub message: String,
}

/// Request to show a picker (list selection).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PickerRequest {
    pub items: Vec<String>,
    pub prompt: Option<String>,
}

/// Request to show a notification to the user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotifyRequest {
    pub message: String,
    #[serde(default)]
    pub level: NotifyLevel,
}

/// Severity level for notifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum NotifyLevel {
    #[default]
    Info,
    Warn,
    Error,
}

// ---------------------------------------------------------------------------
// Split requests
// ---------------------------------------------------------------------------

/// Direction for split and navigation operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SplitDirection {
    Up,
    Down,
    Left,
    Right,
}

/// Request to split a view, creating a new pane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SplitViewRequest {
    /// Which view to split. If `None`, splits the focused view.
    pub view: Option<ViewHandle>,
    pub direction: SplitDirection,
    /// Document to open in the new split. If `None`, clones the current document.
    pub document: Option<DocumentHandle>,
}

/// Request to navigate focus to an adjacent split.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FocusDirectionRequest {
    pub direction: SplitDirection,
}

/// Request to swap the focused view with an adjacent one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapSplitRequest {
    pub direction: SplitDirection,
}

/// Request to transpose the parent container's layout (H↔V).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransposeSplitRequest {
    /// Which view's parent to transpose. If `None`, uses the focused view.
    pub view: Option<ViewHandle>,
}

/// How to resize a split.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResizeAmount {
    /// Grow by N steps.
    Grow(u16),
    /// Shrink by N steps.
    Shrink(u16),
}

/// Which axis to resize.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResizeDimension {
    Width,
    Height,
}

/// Request to resize the focused split.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResizeSplitRequest {
    pub view: Option<ViewHandle>,
    pub dimension: ResizeDimension,
    pub amount: ResizeAmount,
}

// ---------------------------------------------------------------------------
// Tab requests
// ---------------------------------------------------------------------------

/// Request to open a document as a new tab in a view's tab group.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenTabRequest {
    /// Which view's tab group to add to. If `None`, uses the focused view.
    pub view: Option<ViewHandle>,
    pub document: DocumentHandle,
    /// If true, immediately focus the new tab.
    #[serde(default = "default_true")]
    pub focus: bool,
}

/// Request to close a tab.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloseTabRequest {
    pub view: Option<ViewHandle>,
    /// 0-based index of the tab to close. If `None`, closes the active tab.
    pub index: Option<usize>,
}

/// Request to focus a specific tab by index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FocusTabRequest {
    pub view: Option<ViewHandle>,
    pub index: usize,
}

/// Direction for tab cycling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TabCycleDirection {
    Next,
    Previous,
}

/// Request to cycle to the next or previous tab.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CycleTabRequest {
    pub view: Option<ViewHandle>,
    pub direction: TabCycleDirection,
}

// ---------------------------------------------------------------------------
// Float requests
// ---------------------------------------------------------------------------

/// How a floating window is positioned.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FloatPlacement {
    /// Centered in the editor area.
    Centered { width: u16, height: u16 },
    /// Absolute position within the terminal.
    Absolute {
        x: u16,
        y: u16,
        width: u16,
        height: u16,
    },
    /// Anchored near a document position (like a hover popup).
    Anchored {
        view: Option<ViewHandle>,
        line: usize,
        column: usize,
        width: u16,
        height: u16,
        #[serde(default)]
        prefer: AnchorPreference,
    },
}

/// Preferred direction for anchored floats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AnchorPreference {
    #[default]
    Below,
    Above,
}

/// What a floating window displays.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FloatContent {
    /// A document view.
    Document(DocumentHandle),
    /// Styled text blocks rendered by the host.
    Blocks(Vec<FloatBlock>),
    /// Plugin-rendered content via callback.
    PluginRender { callback: RenderCallbackHandle },
}

/// A styled text block for float content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FloatBlock {
    pub text: String,
    /// Theme scope name (e.g. "ui.text", "comment").
    pub style: Option<String>,
}

/// Request to create a floating window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateFloatRequest {
    pub title: Option<String>,
    pub placement: FloatPlacement,
    pub content: FloatContent,
    /// If true, the float captures focus immediately.
    #[serde(default = "default_true")]
    pub focus: bool,
    /// If true, clicking outside or pressing Escape closes the float.
    #[serde(default)]
    pub dismissible: bool,
}

/// Request to update a floating window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateFloatRequest {
    pub float: FloatHandle,
    pub title: Option<Option<String>>,
    pub placement: Option<FloatPlacement>,
    pub content: Option<FloatContent>,
}

/// Request to close a floating window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloseFloatRequest {
    pub float: FloatHandle,
}

// ---------------------------------------------------------------------------
// Panel requests
// ---------------------------------------------------------------------------

/// Which side of the editor to dock a panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PanelSide {
    Left,
    #[default]
    Right,
    Bottom,
}

/// Panel sizing specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PanelSizeSpec {
    /// Exact number of cells.
    Fixed(u16),
    /// Percentage of the editor area (0..=100).
    Percent(u8),
}

/// Request to register a new panel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelRegistration {
    pub title: String,
    #[serde(default)]
    pub side: PanelSide,
    /// Size specification. If `None`, uses a host default.
    pub size: Option<PanelSizeSpec>,
    /// If true, the panel starts hidden.
    #[serde(default)]
    pub hidden: bool,
}

/// Request to update an existing panel's properties.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelUpdateRequest {
    pub panel: PanelHandle,
    pub title: Option<String>,
}

/// Request to close a panel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelCloseRequest {
    pub panel: PanelHandle,
}

/// Request to toggle a panel's visibility.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TogglePanelRequest {
    pub panel: PanelHandle,
}

/// Request to focus a panel (give it input focus).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FocusPanelRequest {
    pub panel: PanelHandle,
}

/// Request to resize a panel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResizePanelRequest {
    pub panel: PanelHandle,
    pub size: PanelSizeSpec,
}

// ---------------------------------------------------------------------------
// Command requests
// ---------------------------------------------------------------------------

/// Definition for registering a new plugin command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandDefinition {
    pub name: String,
    pub doc: Option<String>,
    pub args: Option<Vec<String>>,
}

/// Request to run a command by name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunCommandRequest {
    pub name: String,
    #[serde(default)]
    pub args: Vec<String>,
}

/// Request to update a plugin command's registration metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandUpdateRequest {
    pub command: CommandHandle,
    pub name: Option<String>,
    pub doc: Option<String>,
    pub args: Option<Vec<String>>,
}

/// Request to remove a plugin command registration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandRemoveRequest {
    pub command: CommandHandle,
}

// ---------------------------------------------------------------------------
// Status line
// ---------------------------------------------------------------------------

/// Request to set the status line message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetStatusRequest {
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;

    fn doc(id: u64) -> DocumentHandle {
        DocumentHandle::from_raw(NonZeroU64::new(id).unwrap())
    }

    fn command(id: u64) -> CommandHandle {
        CommandHandle::from_raw(NonZeroU64::new(id).unwrap())
    }

    #[test]
    fn apply_edit_serde() {
        let req = ApplyEditRequest {
            document: doc(1),
            edits: vec![TextEdit {
                start: Position { line: 0, column: 0 },
                end: Position { line: 0, column: 0 },
                new_text: "hello".into(),
            }],
        };
        let bytes = super::super::codec::encode(&req).unwrap();
        let req2: ApplyEditRequest = super::super::codec::decode(&bytes).unwrap();
        assert_eq!(req2.edits.len(), 1);
        assert_eq!(req2.edits[0].new_text, "hello");
    }

    #[test]
    fn notify_level_default() {
        // msgpack with named fields preserves default semantics
        let req = NotifyRequest {
            message: "hi".into(),
            level: NotifyLevel::default(),
        };
        let bytes = super::super::codec::encode(&req).unwrap();
        let req2: NotifyRequest = super::super::codec::decode(&bytes).unwrap();
        assert_eq!(req2.level, NotifyLevel::Info);
        assert_eq!(req2.message, "hi");
    }

    #[test]
    fn panel_side_default() {
        let reg = PanelRegistration {
            title: "Test".into(),
            side: PanelSide::default(),
            size: Some(PanelSizeSpec::Fixed(30)),
            hidden: false,
        };
        let bytes = super::super::codec::encode(&reg).unwrap();
        let reg2: PanelRegistration = super::super::codec::decode(&bytes).unwrap();
        assert_eq!(reg2.side, PanelSide::Right);
    }

    #[test]
    fn split_view_request_serde() {
        let req = SplitViewRequest {
            view: None,
            direction: SplitDirection::Right,
            document: Some(doc(1)),
        };
        let bytes = super::super::codec::encode(&req).unwrap();
        let req2: SplitViewRequest = super::super::codec::decode(&bytes).unwrap();
        assert_eq!(req2.direction, SplitDirection::Right);
        assert!(req2.view.is_none());
    }

    #[test]
    fn float_placement_serde() {
        let req = CreateFloatRequest {
            title: Some("Test".into()),
            placement: FloatPlacement::Centered {
                width: 60,
                height: 20,
            },
            content: FloatContent::Blocks(vec![FloatBlock {
                text: "hello".into(),
                style: Some("ui.text".into()),
            }]),
            focus: true,
            dismissible: true,
        };
        let bytes = super::super::codec::encode(&req).unwrap();
        let req2: CreateFloatRequest = super::super::codec::decode(&bytes).unwrap();
        assert_eq!(req2.title.as_deref(), Some("Test"));
        assert!(req2.dismissible);
    }

    #[test]
    fn tab_request_serde() {
        let req = OpenTabRequest {
            view: None,
            document: doc(3),
            focus: true,
        };
        let bytes = super::super::codec::encode(&req).unwrap();
        let req2: OpenTabRequest = super::super::codec::decode(&bytes).unwrap();
        assert!(req2.focus);
    }

    #[test]
    fn command_update_remove_serde() {
        let update = CommandUpdateRequest {
            command: command(7),
            name: Some("format-buffer".into()),
            doc: Some("Format the current buffer".into()),
            args: Some(vec!["range".into()]),
        };
        let bytes = super::super::codec::encode(&update).unwrap();
        let decoded: CommandUpdateRequest = super::super::codec::decode(&bytes).unwrap();
        assert_eq!(decoded.command, command(7));
        assert_eq!(decoded.name.as_deref(), Some("format-buffer"));

        let remove = CommandRemoveRequest {
            command: command(7),
        };
        let bytes = super::super::codec::encode(&remove).unwrap();
        let decoded: CommandRemoveRequest = super::super::codec::decode(&bytes).unwrap();
        assert_eq!(decoded.command, command(7));
    }
}
