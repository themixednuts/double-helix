use std::borrow::Cow;

use crate::view::ComponentViewState;
use crate::{Document, DocumentId, View, ViewId};

use super::Location;
use super::SurfaceId;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Kind(Cow<'static, str>);

impl Kind {
    #[must_use]
    pub const fn core(name: &'static str) -> Self {
        Self(Cow::Borrowed(name))
    }

    #[must_use]
    pub fn new(name: impl Into<Cow<'static, str>>) -> Self {
        Self(name.into())
    }
}

pub mod kind {
    use super::Kind;

    pub const EDITOR: Kind = Kind::core("editor");
    pub const ASSISTANT_THREAD: Kind = Kind::core("assistant.thread");
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Editor,
    Auxiliary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    New,
    Path(std::path::PathBuf),
    Location(Location),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Open {
    pub target: Target,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capture {
    Selection,
    Symbol,
}

#[derive(Clone)]
pub struct Surface {
    pub id: SurfaceId,
    pub kind: Kind,
    pub role: Role,
    pub view: ViewId,
    pub doc: DocumentId,
}

impl std::fmt::Debug for Surface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Surface")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("role", &self.role)
            .field("view", &self.view)
            .field("doc", &self.doc)
            .finish()
    }
}

fn capture_selection(view_id: ViewId, doc: &Document) -> Option<crate::assistant::context::Kind> {
    let path = doc.path()?.to_path_buf();
    let text = doc.text().slice(..);
    let selection = doc.selection(view_id).primary();
    let content = selection.fragment(text).to_string();
    if content.trim().is_empty() {
        return None;
    }
    let label = content
        .lines()
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string);
    Some(crate::assistant::context::Kind::Selection(
        crate::assistant::context::Selection {
            path,
            range: None,
            text: content,
            label,
        },
    ))
}

fn capture_location(editor: &crate::Editor, view_id: ViewId, doc: &Document) -> Option<Location> {
    let path = doc.path()?.to_path_buf();
    let mut location = Location::new(path, super::location::Source::Cursor)
        .with_range(doc.selection(view_id).primary());
    if let Some(surface) = editor.surface_registry.get_by_view(view_id) {
        location = location.on_surface(surface);
    }
    Some(location)
}

fn capture_symbol(
    _editor: &crate::Editor,
    view_id: ViewId,
    doc: &Document,
) -> Option<crate::assistant::context::Kind> {
    let selection = capture_selection(view_id, doc)?;
    let crate::assistant::context::Kind::Selection(selection) = selection else {
        return None;
    };
    let name = selection
        .text
        .lines()
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())?;
    Some(crate::assistant::context::Kind::Symbol(
        crate::assistant::context::Symbol {
            path: selection.path,
            name: name.to_string(),
            kind: Cow::Borrowed("symbol"),
            range: Some(
                Location::new(doc.path()?.to_path_buf(), super::location::Source::Cursor)
                    .with_range(doc.selection(view_id).primary()),
            ),
            text: selection.text,
            breadcrumb: Vec::new(),
        },
    ))
}

pub enum Ref<'a> {
    Tree {
        view: &'a View,
        doc: &'a Document,
    },
    Component {
        view: &'a ComponentViewState,
        doc: &'a Document,
    },
}

pub enum Mut<'a> {
    Tree {
        view: &'a mut View,
        doc: &'a mut Document,
    },
    Component {
        view: &'a mut ComponentViewState,
        doc: &'a mut Document,
    },
}

fn presence_label(editor: &crate::Editor, participant: super::ParticipantId) -> String {
    editor
        .participant(participant)
        .map(|participant| participant.name.clone())
        .unwrap_or_else(|| format!("participant-{}", participant.value().get()))
}

pub(crate) fn presence_annotations(
    editor: &crate::Editor,
    presence: &[super::Presence],
) -> Vec<crate::document::PluginAnnotation> {
    let mut annotations = Vec::new();
    for item in presence {
        let label = presence_label(editor, item.participant);
        if let Some(cursor) = item.cursor {
            annotations.push(crate::document::PluginAnnotation {
                char_idx: cursor.head,
                text: format!(" {}", label),
                style: Some("ui.virtual.inlay-hint".to_string()),
                fg: None,
                bg: None,
                offset: 1,
                is_line: false,
                virt_line_idx: None,
                dropped_text: Some(" *".to_string()),
            });
        }
        if let Some(selection) = item.selection {
            annotations.push(crate::document::PluginAnnotation {
                char_idx: selection.anchor.min(selection.head),
                text: format!(" selection: {}", label),
                style: Some("ui.text.info".to_string()),
                fg: None,
                bg: None,
                offset: 0,
                is_line: true,
                virt_line_idx: Some(0),
                dropped_text: Some(format!("> {}", label)),
            });
        }
        if item.cursor.is_none() && item.selection.is_none() {
            if let Some(viewport) = item.viewport {
                annotations.push(crate::document::PluginAnnotation {
                    char_idx: viewport.anchor,
                    text: format!(" {} viewport", label),
                    style: Some("ui.text.inactive".to_string()),
                    fg: None,
                    bg: None,
                    offset: 0,
                    is_line: true,
                    virt_line_idx: Some(0),
                    dropped_text: Some(format!("> {}", label)),
                });
            }
        }
    }
    annotations
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("surface not found")]
pub struct Missing {
    pub id: SurfaceId,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Open(#[from] anyhow::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("unknown surface kind: {0:?}")]
    UnknownKind(Kind),
    #[error(transparent)]
    Factory(#[from] Error),
}

pub trait Reveal: sealed::Sealed {
    fn reveal(&mut self, editor: &mut crate::Editor, location: &Location) -> anyhow::Result<()>;
}

pub trait PauseFollow: sealed::Sealed {
    fn pause(&self, event: &crate::editor::EditorEvent) -> Option<super::FollowPause>;
}

pub trait ShowPresence: sealed::Sealed {
    fn show_presence(&mut self, editor: &mut crate::Editor, presence: &[super::Presence]);
}

pub trait Context: sealed::Sealed {
    fn capture(
        &self,
        editor: &crate::Editor,
        capture: Capture,
    ) -> Option<crate::assistant::context::Kind>;
}

pub trait Factory: Send + Sync {
    fn kind(&self) -> Kind;
    fn role(&self) -> Role;
    fn open(&self, editor: &mut crate::Editor, open: Open) -> Result<SurfaceId, Error>;
}

mod sealed {
    pub trait Sealed {}
}

impl sealed::Sealed for Ref<'_> {}
impl sealed::Sealed for Mut<'_> {}

impl Context for Ref<'_> {
    fn capture(
        &self,
        editor: &crate::Editor,
        capture: Capture,
    ) -> Option<crate::assistant::context::Kind> {
        match self {
            Self::Tree { view, doc } => match capture {
                Capture::Selection => capture_selection(view.id, doc).map(|kind| match kind {
                    crate::assistant::context::Kind::Selection(mut selection) => {
                        selection.range = capture_location(editor, view.id, doc);
                        crate::assistant::context::Kind::Selection(selection)
                    }
                    other => other,
                }),
                Capture::Symbol => capture_symbol(editor, view.id, doc),
            },
            Self::Component { view, doc } => match capture {
                Capture::Selection => capture_selection(view.id, doc).map(|kind| match kind {
                    crate::assistant::context::Kind::Selection(mut selection) => {
                        selection.range = capture_location(editor, view.id, doc);
                        crate::assistant::context::Kind::Selection(selection)
                    }
                    other => other,
                }),
                Capture::Symbol => capture_symbol(editor, view.id, doc),
            },
        }
    }
}

impl PauseFollow for Ref<'_> {
    fn pause(&self, event: &crate::editor::EditorEvent) -> Option<super::FollowPause> {
        match event {
            crate::editor::EditorEvent::CursorMoved => Some(super::FollowPause::LocalMove),
            crate::editor::EditorEvent::Scrolled => Some(super::FollowPause::LocalScroll),
            crate::editor::EditorEvent::Edited => Some(super::FollowPause::LocalEdit),
            crate::editor::EditorEvent::BufferSwitched => Some(super::FollowPause::BufferSwitch),
            _ => None,
        }
    }
}

impl ShowPresence for Mut<'_> {
    fn show_presence(&mut self, editor: &mut crate::Editor, presence: &[super::Presence]) {
        let annotations = presence_annotations(editor, presence);
        match self {
            Self::Tree { view, doc } => doc.set_presence_annotations(view.id, annotations),
            Self::Component { view, doc } => doc.set_presence_annotations(view.id, annotations),
        }
    }
}
