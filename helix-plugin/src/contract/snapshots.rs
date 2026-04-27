//! Immutable snapshot types for the host-agnostic plugin contract.
//!
//! Snapshots represent point-in-time views of editor state. They are
//! serializable, immutable, and carry no references to live editor internals.
//! The host builds snapshots from internal state when a plugin requests them
//! via [`super::host::PluginQueryHost`].

use serde::{Deserialize, Serialize};

use super::handles::{DocumentHandle, FloatHandle, PanelHandle, ThreadHandle, ViewHandle};
use super::requests::PanelSide;

// ---------------------------------------------------------------------------
// Primitives
// ---------------------------------------------------------------------------

/// A zero-based line + column position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    /// Zero-based line index.
    pub line: usize,
    /// Zero-based column (character offset within the line).
    pub column: usize,
}

/// A selection range expressed as anchor + head positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionRange {
    pub anchor: Position,
    pub head: Position,
}

/// The editor's current editing mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum EditMode {
    #[default]
    Normal,
    Insert,
    Select,
}

/// Viewport geometry for a view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewportInfo {
    pub first_visible_line: usize,
    pub height: usize,
    pub width: usize,
}

/// An RGB color value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// Severity level for a diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Info,
    Hint,
}

/// A single diagnostic entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub start: Position,
    pub end: Position,
    pub message: String,
    pub severity: DiagnosticSeverity,
}

// ---------------------------------------------------------------------------
// Snapshots
// ---------------------------------------------------------------------------

/// Immutable snapshot of a document's public state.
///
/// Does **not** include the full text content — use
/// [`super::host::PluginQueryHost::document_text`] for that. This keeps
/// snapshots lightweight for the common case of checking metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewSnapshot {
    pub handle: ViewHandle,
    pub document: DocumentHandle,
    pub cursor: Position,
    pub viewport: ViewportInfo,
}

/// Immutable snapshot of workspace-level state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    pub focused_document: Option<DocumentHandle>,
    pub focused_view: Option<ViewHandle>,
    pub documents: Vec<DocumentHandle>,
    pub views: Vec<ViewHandle>,
    pub mode: EditMode,
}

/// Immutable snapshot of theme colors relevant to plugins.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeSnapshot {
    pub name: String,
    pub bg: Option<Color>,
    pub fg: Option<Color>,
    pub selection: Option<Color>,
    pub cursor: Option<Color>,
}

/// Immutable snapshot of diagnostics for a document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticSnapshot {
    pub document: DocumentHandle,
    pub diagnostics: Vec<Diagnostic>,
}

// ---------------------------------------------------------------------------
// Split tree snapshots
// ---------------------------------------------------------------------------

/// Direction children are laid out in a split container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SplitLayoutDirection {
    /// Children arranged left-to-right (vertical dividers).
    Horizontal,
    /// Children arranged top-to-bottom (horizontal dividers).
    Vertical,
}

/// Recursive snapshot of the split tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SplitNodeSnapshot {
    /// A leaf view in the split tree.
    Leaf { view: ViewHandle },
    /// A container with child splits.
    Container {
        direction: SplitLayoutDirection,
        children: Vec<SplitNodeSnapshot>,
    },
}

/// Snapshot of the entire split tree topology.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SplitTreeSnapshot {
    pub root: SplitNodeSnapshot,
}

// ---------------------------------------------------------------------------
// Tab snapshots
// ---------------------------------------------------------------------------

/// Snapshot of a single tab within a view's tab group.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabSnapshot {
    pub document: DocumentHandle,
    pub title: String,
    pub is_modified: bool,
}

/// Snapshot of a view's tab group.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabGroupSnapshot {
    pub view: ViewHandle,
    pub tabs: Vec<TabSnapshot>,
    pub active: usize,
}

// ---------------------------------------------------------------------------
// Float snapshots
// ---------------------------------------------------------------------------

/// Snapshot of a floating window's area.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AreaSnapshot {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

/// Snapshot of a floating window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FloatSnapshot {
    pub handle: FloatHandle,
    pub title: Option<String>,
    pub area: AreaSnapshot,
    pub is_focused: bool,
}

// ---------------------------------------------------------------------------
// Panel snapshots
// ---------------------------------------------------------------------------

/// Snapshot of a registered panel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelSnapshot {
    pub handle: PanelHandle,
    pub title: String,
    pub side: PanelSide,
    pub visible: bool,
    pub is_focused: bool,
}

// ---------------------------------------------------------------------------
// Focus target snapshot
// ---------------------------------------------------------------------------

/// What currently has input focus.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FocusTargetSnapshot {
    /// The main editor (split tree view).
    Editor,
    /// A docked panel.
    Panel(PanelHandle),
    /// A floating window.
    Float(FloatHandle),
    /// A modal layer (prompt, picker, etc.).
    Layer,
}

// ---------------------------------------------------------------------------
// Extended workspace snapshot
// ---------------------------------------------------------------------------

/// Full workspace snapshot including UI topology.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceDetailSnapshot {
    pub focused_document: Option<DocumentHandle>,
    pub focused_view: Option<ViewHandle>,
    pub documents: Vec<DocumentHandle>,
    pub views: Vec<ViewHandle>,
    pub mode: EditMode,
    pub splits: SplitTreeSnapshot,
    pub panels: Vec<PanelSnapshot>,
    pub floats: Vec<FloatSnapshot>,
    pub focus: FocusTargetSnapshot,
}

// ---------------------------------------------------------------------------
// Assistant snapshots
// ---------------------------------------------------------------------------

/// Current run state of an assistant thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssistantRunState {
    Idle,
    Running,
    Waiting,
    Failed,
}

/// Follow state for an assistant thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssistantFollowState {
    Off,
    On,
    Paused,
}

/// Snapshot of an assistant thread's state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantThreadSnapshot {
    pub handle: ThreadHandle,
    pub title: Option<String>,
    pub run: AssistantRunState,
    pub entry_count: usize,
    pub has_context: bool,
    pub is_active: bool,
    pub scope_cwd: String,
    pub follow: AssistantFollowState,
}

/// Snapshot of the assistant system as a whole.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantSnapshot {
    pub active_thread: Option<ThreadHandle>,
    pub threads: Vec<AssistantThreadSnapshot>,
    pub is_ready: bool,
}

/// A single entry in an assistant thread.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantEntrySnapshot {
    pub id: u64,
    pub kind: String,
    pub text: Option<String>,
    pub location_count: usize,
}

/// A context item attached to an assistant thread.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantContextSnapshot {
    pub id: String,
    pub kind: String,
    pub label: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;

    fn doc_handle(id: u64) -> DocumentHandle {
        DocumentHandle::from_raw(NonZeroU64::new(id).unwrap())
    }

    #[test]
    fn document_snapshot_serde_round_trip() {
        let snap = DocumentSnapshot {
            handle: doc_handle(1),
            path: Some("/tmp/test.rs".into()),
            language: Some("rust".into()),
            is_modified: false,
            line_count: 42,
            selections: vec![SelectionRange {
                anchor: Position { line: 0, column: 0 },
                head: Position { line: 0, column: 5 },
            }],
            mode: EditMode::Normal,
        };
        let bytes = super::super::codec::encode(&snap).unwrap();
        let snap2: DocumentSnapshot = super::super::codec::decode(&bytes).unwrap();
        assert_eq!(snap.handle, snap2.handle);
        assert_eq!(snap.line_count, snap2.line_count);
        assert_eq!(snap.selections.len(), snap2.selections.len());
    }

    #[test]
    fn edit_mode_default() {
        assert_eq!(EditMode::default(), EditMode::Normal);
    }

    fn view_handle(id: u64) -> ViewHandle {
        ViewHandle::from_raw(NonZeroU64::new(id).unwrap())
    }

    fn float_handle(id: u64) -> FloatHandle {
        FloatHandle::from_raw(NonZeroU64::new(id).unwrap())
    }

    fn panel_handle(id: u64) -> PanelHandle {
        PanelHandle::from_raw(NonZeroU64::new(id).unwrap())
    }

    #[test]
    fn split_tree_snapshot_serde() {
        let snap = SplitTreeSnapshot {
            root: SplitNodeSnapshot::Container {
                direction: SplitLayoutDirection::Horizontal,
                children: vec![
                    SplitNodeSnapshot::Leaf {
                        view: view_handle(1),
                    },
                    SplitNodeSnapshot::Leaf {
                        view: view_handle(2),
                    },
                ],
            },
        };
        let bytes = super::super::codec::encode(&snap).unwrap();
        let snap2: SplitTreeSnapshot = super::super::codec::decode(&bytes).unwrap();
        match &snap2.root {
            SplitNodeSnapshot::Container {
                direction,
                children,
            } => {
                assert_eq!(*direction, SplitLayoutDirection::Horizontal);
                assert_eq!(children.len(), 2);
            }
            _ => panic!("expected Container"),
        }
    }

    #[test]
    fn tab_group_snapshot_serde() {
        let snap = TabGroupSnapshot {
            view: view_handle(1),
            tabs: vec![
                TabSnapshot {
                    document: doc_handle(1),
                    title: "main.rs".into(),
                    is_modified: false,
                },
                TabSnapshot {
                    document: doc_handle(2),
                    title: "lib.rs".into(),
                    is_modified: true,
                },
            ],
            active: 0,
        };
        let bytes = super::super::codec::encode(&snap).unwrap();
        let snap2: TabGroupSnapshot = super::super::codec::decode(&bytes).unwrap();
        assert_eq!(snap2.tabs.len(), 2);
        assert_eq!(snap2.active, 0);
        assert!(snap2.tabs[1].is_modified);
    }

    #[test]
    fn float_snapshot_serde() {
        let snap = FloatSnapshot {
            handle: float_handle(1),
            title: Some("Preview".into()),
            area: AreaSnapshot {
                x: 10,
                y: 5,
                width: 60,
                height: 20,
            },
            is_focused: true,
        };
        let bytes = super::super::codec::encode(&snap).unwrap();
        let snap2: FloatSnapshot = super::super::codec::decode(&bytes).unwrap();
        assert_eq!(snap2.area.width, 60);
        assert!(snap2.is_focused);
    }

    #[test]
    fn panel_snapshot_serde() {
        let snap = PanelSnapshot {
            handle: panel_handle(1),
            title: "Files".into(),
            side: PanelSide::Left,
            visible: true,
            is_focused: false,
        };
        let bytes = super::super::codec::encode(&snap).unwrap();
        let snap2: PanelSnapshot = super::super::codec::decode(&bytes).unwrap();
        assert_eq!(snap2.title, "Files");
        assert_eq!(snap2.side, PanelSide::Left);
    }

    #[test]
    fn focus_target_snapshot_serde() {
        let targets = vec![
            FocusTargetSnapshot::Editor,
            FocusTargetSnapshot::Panel(panel_handle(1)),
            FocusTargetSnapshot::Float(float_handle(2)),
            FocusTargetSnapshot::Layer,
        ];
        for target in targets {
            let bytes = super::super::codec::encode(&target).unwrap();
            let _: FocusTargetSnapshot = super::super::codec::decode(&bytes).unwrap();
        }
    }
}
