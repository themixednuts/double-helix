//! Engine-only embedding API.
//!
//! This module is for hosts that want Helix's editor state, modal editing
//! engines, commands, documents, selections, diagnostics, and runtime plumbing,
//! but want to provide their own UI. It intentionally does not depend on
//! `helix-term` or any terminal renderer.

use std::{borrow::Cow, collections::HashMap, path::Path, sync::Arc};

use arc_swap::{access::DynAccess, ArcSwap};
use helix_core::{syntax, Rope, Selection};
use helix_runtime::Runtime;

use crate::{
    commands::editing,
    document::{Document, Mode},
    engine::{
        EditingEngine, EditingEngineFactory, EngineResult, HeadlessEditingEngineFactory,
        ModalInputState,
    },
    graphics::Rect,
    handlers::Handlers,
    input::KeyEvent,
    keymap::{ModalKeyTrie, ModalKeymaps},
    theme, DocumentId, Editor, View, ViewId,
};

use super::{Config, EditingEngineConfig, EditorBuilder, Severity};

/// Where entering insert mode should place the cursor relative to selections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertPlacement {
    /// Equivalent to Helix's `insert_mode`: insert before each selection.
    BeforeSelection,
    /// Equivalent to Helix's `append_mode`: append after each selection.
    AfterSelection,
}

impl InsertPlacement {
    const fn command_name(self) -> &'static str {
        match self {
            Self::BeforeSelection => "insert_mode",
            Self::AfterSelection => "append_mode",
        }
    }
}

/// Result of dispatching a key through an [`EditorSession`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorSessionEvent {
    /// An editor command ran or editor state changed.
    Executed,
    /// The engine consumed the key and is waiting for more input.
    Pending,
    /// A character was inserted directly in insert mode.
    Inserted(char),
    /// Dot-repeat replayed an insert sequence.
    ReplayedInsert { inserted: usize },
    /// The key was not bound by the session keymaps or engine.
    Unbound(KeyEvent),
    /// The session has no focused view/document yet.
    NoFocusedView,
    /// Dot-repeat referenced an insert entry command this session does not own.
    UnsupportedInsertReplay,
}

/// Builds an [`EditorSession`] for native GUI/TUI hosts.
pub struct EditorSessionBuilder {
    editor: EditorBuilder,
    engine_factory: Arc<dyn EditingEngineFactory>,
    engine_config: EditingEngineConfig,
    keymaps: Arc<ArcSwap<HashMap<Mode, ModalKeyTrie>>>,
}

impl EditorSessionBuilder {
    #[must_use]
    pub fn new(area: Rect, runtime: Runtime) -> Self {
        Self {
            editor: EditorBuilder::new(area, runtime),
            engine_factory: Arc::new(HeadlessEditingEngineFactory),
            engine_config: EditingEngineConfig::default(),
            keymaps: Arc::new(ArcSwap::from_pointee(HashMap::new())),
        }
    }

    #[must_use]
    pub fn theme_loader(mut self, theme_loader: Arc<theme::Loader>) -> Self {
        self.editor = self.editor.theme_loader(theme_loader);
        self
    }

    #[must_use]
    pub fn language_loader(mut self, language_loader: syntax::Loader) -> Self {
        self.editor = self.editor.language_loader(language_loader);
        self
    }

    #[must_use]
    pub fn language_loader_store(mut self, language_loader: Arc<ArcSwap<syntax::Loader>>) -> Self {
        self.editor = self.editor.language_loader_store(language_loader);
        self
    }

    #[must_use]
    pub fn config(mut self, config: Config) -> Self {
        self.engine_config = config.editing_engine;
        self.editor = self.editor.config(config);
        self
    }

    #[must_use]
    pub fn config_access(mut self, config: Arc<dyn DynAccess<Config> + Send + Sync>) -> Self {
        self.editor = self.editor.config_access(config);
        self
    }

    #[must_use]
    pub fn handlers(mut self, handlers: Handlers) -> Self {
        self.editor = self.editor.handlers(handlers);
        self
    }

    #[must_use]
    pub fn engine_factory(mut self, engine_factory: Arc<dyn EditingEngineFactory>) -> Self {
        self.engine_factory = engine_factory;
        self
    }

    #[must_use]
    pub fn engine_config(mut self, engine_config: EditingEngineConfig) -> Self {
        self.engine_config = engine_config;
        self
    }

    #[must_use]
    pub fn modal_keymaps(mut self, keymaps: HashMap<Mode, ModalKeyTrie>) -> Self {
        self.keymaps = Arc::new(ArcSwap::from_pointee(keymaps));
        self
    }

    #[must_use]
    pub fn modal_keymap_store(
        mut self,
        keymaps: Arc<ArcSwap<HashMap<Mode, ModalKeyTrie>>>,
    ) -> Self {
        self.keymaps = keymaps;
        self
    }

    #[must_use]
    pub fn build(self) -> EditorSession {
        let mut editor = self.editor.build();
        editor.frontend_mut().engine_factory = Arc::clone(&self.engine_factory);
        editor.frontend_mut().modal_keymaps = Arc::clone(&self.keymaps);
        let keymaps = ModalKeymaps::from_shared(self.keymaps);
        let engine = self.engine_factory.create(self.engine_config);
        EditorSession {
            editor,
            engine,
            keymaps,
        }
    }
}

/// Editor state plus a modal editing engine, without a renderer or terminal app.
pub struct EditorSession {
    editor: Editor,
    engine: Box<dyn EditingEngine>,
    keymaps: ModalKeymaps,
}

impl EditorSession {
    pub fn editor(&self) -> &Editor {
        &self.editor
    }

    pub fn editor_mut(&mut self) -> &mut Editor {
        &mut self.editor
    }

    pub fn into_editor(self) -> Editor {
        self.editor
    }

    pub fn engine(&self) -> &dyn EditingEngine {
        self.engine.as_ref()
    }

    pub fn keymaps(&self) -> &ModalKeymaps {
        &self.keymaps
    }

    #[must_use]
    pub fn mode(&self) -> Mode {
        self.editor.mode()
    }

    pub fn engine_mode_name(&self) -> &str {
        self.engine.mode_name()
    }

    pub fn pending_display(&self) -> &str {
        self.engine.pending_display()
    }

    #[must_use]
    pub fn modal_input_state(&self) -> ModalInputState {
        self.engine.input_state()
    }

    #[must_use]
    pub fn snapshot(&self) -> EditorSnapshot<'_> {
        EditorSnapshot {
            editor: &self.editor,
        }
    }

    pub fn dispatch_key(&mut self, key: KeyEvent) -> EditorSessionEvent {
        let key = key.canonicalize();
        let Some((view_id, doc_id)) = self.focused_ids() else {
            return EditorSessionEvent::NoFocusedView;
        };

        let mode_before = self.editor.mode();

        if let Some(result) =
            self.engine
                .pre_resolve(&mut self.editor, view_id, doc_id, &self.keymaps, key)
        {
            let event = self.apply_engine_result(key, result);
            self.finish_dispatch(mode_before);
            return event;
        }

        let mode = self.editor.mode();
        let lookup = self.keymaps.get(mode, key);
        let result = self.engine.process_lookup(
            &mut self.editor,
            view_id,
            doc_id,
            &mut self.keymaps,
            key,
            lookup,
        );
        let event = self.apply_engine_result(key, result);
        self.finish_dispatch(mode_before);
        event
    }

    pub fn enter_insert_mode(&mut self, placement: InsertPlacement) -> EditorSessionEvent {
        let Some((view_id, doc_id)) = self.focused_ids() else {
            return EditorSessionEvent::NoFocusedView;
        };
        let mode_before = self.editor.mode();
        self.apply_insert_placement(placement, view_id, doc_id);
        self.finish_mode_change(mode_before, Some(placement.command_name()));
        EditorSessionEvent::Executed
    }

    pub fn enter_normal_mode(&mut self) -> EditorSessionEvent {
        let mode_before = self.editor.mode();
        self.editor.enter_normal_mode();
        self.finish_mode_change(mode_before, None);
        self.engine.reset();
        crate::engine::KeymapQuery::clear_sticky(&mut self.keymaps);
        self.editor.frontend_mut().focused_modal_input = self.engine.input_state();
        EditorSessionEvent::Executed
    }

    pub fn insert_char(&mut self, ch: char) -> EditorSessionEvent {
        let Some((view_id, doc_id)) = self.focused_ids() else {
            return EditorSessionEvent::NoFocusedView;
        };
        if editing::insert_char(&mut self.editor, view_id, doc_id, ch) {
            self.editor.mark_redraw_pending();
        }
        EditorSessionEvent::Inserted(ch)
    }

    fn focused_ids(&self) -> Option<(ViewId, DocumentId)> {
        let view = self.editor.tree.try_get(self.editor.tree.focus)?;
        Some((view.id, view.doc))
    }

    fn finish_dispatch(&mut self, mode_before: Mode) {
        self.finish_mode_change(mode_before, None);
        self.editor.frontend_mut().focused_modal_input = self.engine.input_state();
    }

    fn finish_mode_change(&mut self, mode_before: Mode, command_name: Option<&'static str>) {
        let mode_after = self.editor.mode();
        if mode_after != mode_before {
            if mode_after == Mode::Insert && mode_before != Mode::Insert {
                let entry = command_name
                    .map(Cow::Borrowed)
                    .unwrap_or(Cow::Borrowed("insert_mode"));
                self.engine.begin_insert_recording(entry);
            } else if mode_before == Mode::Insert && mode_after != Mode::Insert {
                self.engine.end_insert_recording();
            }
            self.editor.mark_redraw_pending();
        }
    }

    fn apply_engine_result(&mut self, key: KeyEvent, result: EngineResult) -> EditorSessionEvent {
        match result {
            EngineResult::Executed => {
                self.editor.mark_redraw_pending();
                EditorSessionEvent::Executed
            }
            EngineResult::Pending => EditorSessionEvent::Pending,
            EngineResult::InsertChar(ch) => self.insert_char(ch),
            EngineResult::CancelledInsert(keys) => {
                let mut inserted = 0;
                for key in keys {
                    if let Some(ch) = key.char() {
                        if matches!(self.insert_char(ch), EditorSessionEvent::Inserted(_)) {
                            inserted += 1;
                        }
                    }
                }
                EditorSessionEvent::ReplayedInsert { inserted }
            }
            EngineResult::Unbound => EditorSessionEvent::Unbound(key),
            EngineResult::ReplayInsert {
                entry_command,
                keys,
            } => self.replay_insert(&entry_command, &keys),
        }
    }

    fn replay_insert(&mut self, entry_command: &str, keys: &[KeyEvent]) -> EditorSessionEvent {
        let Some(placement) = insert_placement_for_command(entry_command) else {
            self.editor
                .set_error(format!("unsupported insert replay entry: {entry_command}"));
            return EditorSessionEvent::UnsupportedInsertReplay;
        };

        if matches!(
            self.enter_insert_mode(placement),
            EditorSessionEvent::NoFocusedView
        ) {
            return EditorSessionEvent::NoFocusedView;
        }

        let mut inserted = 0;
        for key in keys.iter().copied() {
            if self.editor.mode() != Mode::Insert {
                break;
            }
            if matches!(self.dispatch_key(key), EditorSessionEvent::Inserted(_)) {
                inserted += 1;
            }
        }

        if self.editor.mode() == Mode::Insert {
            self.enter_normal_mode();
        }

        EditorSessionEvent::ReplayedInsert { inserted }
    }

    fn apply_insert_placement(
        &mut self,
        placement: InsertPlacement,
        view_id: ViewId,
        doc_id: DocumentId,
    ) {
        match placement {
            InsertPlacement::BeforeSelection => {
                editing::insert_mode(&mut self.editor, view_id, doc_id);
            }
            InsertPlacement::AfterSelection => {
                editing::append_mode(&mut self.editor, view_id, doc_id);
            }
        }
    }
}

fn insert_placement_for_command(command: &str) -> Option<InsertPlacement> {
    match command {
        "insert_mode" => Some(InsertPlacement::BeforeSelection),
        "append_mode" => Some(InsertPlacement::AfterSelection),
        _ => None,
    }
}

/// Borrowed semantic snapshot of an editor session.
#[derive(Clone, Copy)]
pub struct EditorSnapshot<'a> {
    editor: &'a Editor,
}

impl<'a> EditorSnapshot<'a> {
    pub fn editor(&self) -> &'a Editor {
        self.editor
    }

    #[must_use]
    pub fn mode(&self) -> Mode {
        self.editor.mode()
    }

    #[must_use]
    pub fn is_redraw_pending(&self) -> bool {
        self.editor.is_redraw_pending()
    }

    pub fn status(&self) -> Option<StatusSnapshot<'a>> {
        self.editor
            .get_status()
            .map(|(message, severity)| StatusSnapshot {
                message: message.as_ref(),
                severity: *severity,
            })
    }

    pub fn focused(&self) -> Option<FocusedSnapshot<'a>> {
        let view = self.editor.tree.try_get(self.editor.tree.focus)?;
        let document = self.editor.document(view.doc)?;
        let selection = document.selections().get(&view.id)?;
        Some(FocusedSnapshot {
            view,
            document,
            selection,
        })
    }

    pub fn documents(&self) -> impl Iterator<Item = DocumentSnapshot<'a>> + 'a {
        self.editor
            .documents()
            .map(|document| DocumentSnapshot { document })
    }

    pub fn views(&self) -> impl Iterator<Item = ViewSnapshot<'a>> + 'a {
        self.editor
            .tree
            .views()
            .map(|(view, focused)| ViewSnapshot { view, focused })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct StatusSnapshot<'a> {
    message: &'a str,
    severity: Severity,
}

impl<'a> StatusSnapshot<'a> {
    pub fn message(&self) -> &'a str {
        self.message
    }

    #[must_use]
    pub fn severity(&self) -> Severity {
        self.severity
    }
}

#[derive(Clone, Copy)]
pub struct FocusedSnapshot<'a> {
    view: &'a View,
    document: &'a Document,
    selection: &'a Selection,
}

impl<'a> FocusedSnapshot<'a> {
    pub fn view(&self) -> &'a View {
        self.view
    }

    pub fn document(&self) -> &'a Document {
        self.document
    }

    pub fn selection(&self) -> &'a Selection {
        self.selection
    }
}

#[derive(Clone, Copy)]
pub struct DocumentSnapshot<'a> {
    document: &'a Document,
}

impl<'a> DocumentSnapshot<'a> {
    #[must_use]
    pub fn id(&self) -> DocumentId {
        self.document.id()
    }

    pub fn document(&self) -> &'a Document {
        self.document
    }

    pub fn path(&self) -> Option<&'a Path> {
        self.document.path().map(std::path::PathBuf::as_path)
    }

    pub fn text(&self) -> &'a Rope {
        self.document.text()
    }

    #[must_use]
    pub fn is_modified(&self) -> bool {
        self.document.is_modified()
    }

    pub fn selection(&self, view_id: ViewId) -> Option<&'a Selection> {
        self.document.selections().get(&view_id)
    }
}

#[derive(Clone, Copy)]
pub struct ViewSnapshot<'a> {
    view: &'a View,
    focused: bool,
}

impl<'a> ViewSnapshot<'a> {
    #[must_use]
    pub fn id(&self) -> ViewId {
        self.view.id
    }

    #[must_use]
    pub fn document_id(&self) -> DocumentId {
        self.view.doc
    }

    #[must_use]
    pub fn area(&self) -> Rect {
        self.view.area
    }

    #[must_use]
    pub fn is_focused(&self) -> bool {
        self.focused
    }

    pub fn view(&self) -> &'a View {
        self.view
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::Action;
    use crate::input::KeyCode;

    #[test]
    fn session_snapshot_is_empty_before_opening_document() {
        let area = Rect::new(0, 0, 40, 12);
        let runtime = helix_runtime::test::RuntimeTest::default();
        runtime.block_on(async {
            let session = EditorSessionBuilder::new(area, runtime.runtime()).build();
            let snapshot = session.snapshot();

            assert!(snapshot.focused().is_none());
            assert_eq!(snapshot.documents().count(), 0);
            assert_eq!(snapshot.views().count(), 0);
        });
    }

    #[test]
    fn session_insert_char_mutates_focused_document() {
        let area = Rect::new(0, 0, 40, 12);
        let runtime = helix_runtime::test::RuntimeTest::default();
        runtime.block_on(async {
            let mut session = EditorSessionBuilder::new(area, runtime.runtime()).build();
            let doc_id = session.editor_mut().new_file(Action::VerticalSplit);

            assert_eq!(
                session.enter_insert_mode(InsertPlacement::BeforeSelection),
                EditorSessionEvent::Executed
            );
            assert_eq!(session.insert_char('x'), EditorSessionEvent::Inserted('x'));

            let doc = session.editor().document(doc_id).unwrap();
            assert_eq!(
                doc.text().to_string(),
                format!("x{}", doc.line_ending().as_str())
            );
            assert!(session.snapshot().is_redraw_pending());
        });
    }

    #[test]
    fn dispatch_key_without_focused_view_is_typed() {
        let area = Rect::new(0, 0, 40, 12);
        let runtime = helix_runtime::test::RuntimeTest::default();
        runtime.block_on(async {
            let mut session = EditorSessionBuilder::new(area, runtime.runtime()).build();
            let key = KeyEvent {
                code: KeyCode::Char('x'),
                modifiers: crate::keyboard::KeyModifiers::empty(),
            };

            assert_eq!(session.dispatch_key(key), EditorSessionEvent::NoFocusedView);
        });
    }
}
