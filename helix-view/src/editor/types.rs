use std::{collections::BTreeMap, sync::Arc};
use std::{path::PathBuf, time::Duration};

use crate::{document::SavePoint, DocumentId, ViewId};
use helix_core::{diagnostic::DiagnosticProvider, Change, Uri};
use helix_lsp::lsp;

use super::{core::Editor, Config};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SavePolicy {
    Safe,
    Overwrite,
}

impl SavePolicy {
    pub const fn should_overwrite(self) -> bool {
        matches!(self, Self::Overwrite)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClosePolicy {
    ProtectModified,
    DiscardModified,
}

impl ClosePolicy {
    pub const fn should_discard_modified(self) -> bool {
        matches!(self, Self::DiscardModified)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activation {
    Preserve,
    Activate,
}

impl Activation {
    pub const fn should_activate(self) -> bool {
        matches!(self, Self::Activate)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelBehavior {
    Preserve,
    Open,
}

impl PanelBehavior {
    pub const fn should_open(self) -> bool {
        matches!(self, Self::Open)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadSelectPolicy {
    PreserveCurrent,
    ReplaceCurrent,
}

impl ThreadSelectPolicy {
    pub const fn should_replace_current(self) -> bool {
        matches!(self, Self::ReplaceCurrent)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameSelection {
    Preserve,
    SelectFirst,
}

#[derive(Debug, Clone)]
pub struct ShowDocumentRequest {
    pub path: PathBuf,
    pub action: super::Action,
    pub selection: Option<lsp::Range>,
    pub offset_encoding: helix_lsp::OffsetEncoding,
}

#[derive(Debug, Clone)]
pub struct BenchActionUpdate {
    pub category: &'static str,
    pub action_dur: Duration,
    pub reset_dur: Duration,
    pub reset: crate::bench::BenchResetStats,
    pub post_action_lines: usize,
    pub post_action_bytes: usize,
    pub force_insert: bool,
    pub macro_str: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub struct BenchFrameUpdate {
    pub poll_dur: Duration,
    pub total_reset: Duration,
    pub action_dur: Duration,
    pub render_dur: Duration,
    pub tick_dur: Duration,
    pub buf_lines: usize,
    pub buf_bytes: usize,
}

impl FrameSelection {
    pub const fn should_select_first(self) -> bool {
        matches!(self, Self::SelectFirst)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Breakpoint {
    pub id: Option<usize>,
    pub verified: bool,
    pub message: Option<String>,

    pub line: usize,
    pub column: Option<usize>,
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
    pub log_message: Option<String>,
}

pub(super) type Diagnostics = BTreeMap<Uri, Arc<Vec<(lsp::Diagnostic, DiagnosticProvider)>>>;

#[derive(Clone, Copy, Debug)]
pub struct EditTarget {
    pub view_id: ViewId,
    pub doc_id: DocumentId,
}

pub(super) type Motion = Box<dyn Fn(&mut Editor) + Send + Sync>;

#[derive(Debug)]
pub enum EditorEvent {
    CursorMoved,
    Scrolled,
    Edited,
    BufferSwitched,
    Redraw,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AssistantFollowSnapshot {
    pub(crate) doc: DocumentId,
    pub(crate) version: i32,
    pub(crate) cursor: usize,
    pub(crate) scroll: usize,
}

pub struct AssistantUpdateOutcome {
    pub effects: Vec<crate::assistant::effect::Effect>,
    pub permission_request: Option<(
        crate::assistant::thread::Id,
        crate::assistant::permission::Request,
    )>,
}

#[derive(Debug, Clone)]
pub enum ConfigEvent {
    Refresh,
    Update(Box<Config>),
}

#[derive(Debug, Clone)]
pub enum CompleteAction {
    Triggered,
    Selected {
        savepoint: Arc<SavePoint>,
    },
    Applied {
        trigger_offset: usize,
        changes: Vec<Change>,
        placeholder: bool,
    },
}
