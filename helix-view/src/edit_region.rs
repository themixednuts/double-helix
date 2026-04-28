//! A self-contained editing region for UI components.
//!
//! [`EditRegion`] composes a [`crate::content_region::ContentRegion`], a
//! component-owned [`Document`], and per-region [`Mode`]. Components embed an
//! `EditRegion` when they need an editable text area backed by the full
//! editing engine.

use crate::content_region::ContentRegion;
use crate::document::Mode;
use crate::engine::{EditingEngine, ModalInputState};
use crate::graphics::Rect;
use crate::history_state::ViewHistoryState;
use crate::keymap::ModalKeymaps;
use crate::traits::{
    Bounded, Focusable, HistoryViewport, Identified, Jumpable, Modal, NavigableViewport,
    TextViewport,
};
use crate::view::ViewPosition;
use crate::{Document, DocumentId, Editor, ViewId};

/// A composable editing region that components can embed.
///
/// Owns a read-only [`ContentRegion`] for shared viewport/focus behavior and
/// references a component-owned document in `editor.component_docs`. Editable
/// scroll/anchor state is doc-owned via [`TextViewport`], so this type does
/// not implement the generic [`crate::traits::Viewport`] trait and therefore
/// avoids exposing two competing offset models.
pub struct EditRegion {
    region: ContentRegion<()>,
    doc_id: Option<DocumentId>,
    mode: Mode,
    /// Per-region editing engine instance with independent state
    /// (count, pending operators, insert recording).
    engine: Option<Box<dyn EditingEngine>>,
    /// Per-region modal keymaps with independent pending/sticky state.
    keymaps: ModalKeymaps,
    history: ViewHistoryState,
}

impl Default for EditRegion {
    fn default() -> Self {
        Self {
            region: ContentRegion::default(),
            doc_id: None,
            mode: Mode::Normal,
            engine: None,
            keymaps: ModalKeymaps::default(),
            history: ViewHistoryState::new(DocumentId::default()),
        }
    }
}

impl EditRegion {
    pub fn view_id(&self) -> ViewId {
        self.region.view_id()
    }

    /// The document ID backing this region, if initialized.
    pub fn doc_id(&self) -> Option<DocumentId> {
        self.doc_id
    }

    /// Ensure the region has a viewport ID, a backing document, and an engine.
    /// Call this from the component's `sync()`.
    /// The engine has independent state (count, pending, insert recording).
    pub fn ensure_init(&mut self, editor: &mut Editor) {
        self.region.ensure_init(editor);
        if self.doc_id.is_none() {
            let doc = Document::default(editor.config.clone(), editor.syn_loader.clone());
            let id = editor.new_component_doc(doc);
            self.doc_id = Some(id);
            let factory = editor.frontend().engine_factory.clone();
            self.engine = Some(factory.create(editor.config.load().editing_engine));
            self.keymaps = ModalKeymaps::from_shared(editor.frontend().modal_keymaps.clone());
            self.history = ViewHistoryState::new(id);
        }
        if let Some(doc) = self
            .doc_id
            .and_then(|id| editor.component_docs.get_mut(&id))
        {
            doc.ensure_view_init(self.region.id());
        }
        if let Some(doc_id) = self.doc_id {
            let state = editor.ensure_component_view(self.region.id(), doc_id);
            state.area = self.area();
            state.history = self.history.clone();
        }
    }

    /// Get the text content as a string, then clear the document.
    pub fn take_text(&self, editor: &mut Editor) -> Option<String> {
        let doc_id = self.doc_id?;
        let doc = editor.component_docs.get_mut(&doc_id)?;
        let text = doc.text().to_string();
        if text.trim().is_empty() {
            return None;
        }
        // Replace entire content with empty text.
        let len = doc.text().len_chars();
        let selection = helix_core::Selection::point(0);
        doc.set_selection(self.region.id(), selection);
        let transaction = helix_core::Transaction::change(doc.text(), [(0, len, None)].into_iter());
        doc.apply(&transaction, self.region.id());
        Some(text.trim_end().to_string())
    }

    /// Read-only access to the backing document.
    pub fn document<'a>(&self, editor: &'a Editor) -> Option<&'a Document> {
        self.doc_id.and_then(|id| editor.component_docs.get(&id))
    }

    /// Mutable access to the backing document.
    pub fn document_mut<'a>(&self, editor: &'a mut Editor) -> Option<&'a mut Document> {
        self.doc_id
            .and_then(|id| editor.component_docs.get_mut(&id))
    }

    /// Enter insert mode: set the region mode and notify the engine.
    pub fn enter_insert_mode(&mut self, entry_command: std::borrow::Cow<'static, str>) {
        self.mode = Mode::Insert;
        if let Some(engine) = &mut self.engine {
            engine.begin_insert_recording(entry_command);
        }
    }

    /// Exit insert mode: set the region mode back to normal and finalize
    /// the engine's insert recording.
    pub fn exit_insert_mode(&mut self) {
        self.mode = Mode::Normal;
        if let Some(engine) = &mut self.engine {
            engine.end_insert_recording();
        }
    }

    /// Snapshot transient modal input state owned by the region engine.
    pub fn input_state(&self) -> ModalInputState {
        self.engine
            .as_ref()
            .map_or_else(ModalInputState::default, |engine| engine.input_state())
    }

    /// Dispatch a key through the region's own engine + keymaps.
    pub fn dispatch_key(
        &mut self,
        editor: &mut Editor,
        key: crate::input::KeyEvent,
    ) -> Option<crate::engine::EngineResult> {
        let doc_id = self.doc_id?;
        let area = self.area();
        let history = self.history.clone();
        let keymaps = &mut self.keymaps;
        let mut engine = self.engine.take()?;
        let state = editor.ensure_component_view(self.region.id(), doc_id);
        state.doc = doc_id;
        state.area = area;
        state.history = history;

        let global_mode = editor.mode;
        editor.mode = self.mode;

        if let Some(result) = engine.pre_resolve(editor, self.region.id(), doc_id, keymaps, key) {
            self.mode = editor.mode;
            editor.mode = global_mode;
            if self.region.is_focused() {
                editor.frontend_mut().focused_modal_input = engine.input_state();
            }
            self.engine = Some(engine);
            return Some(result);
        }

        let lookup = keymaps.get(editor.mode(), key);
        let result = engine.process_lookup(editor, self.region.id(), doc_id, keymaps, key, lookup);

        self.mode = editor.mode;
        editor.mode = global_mode;
        if self.region.is_focused() {
            editor.frontend_mut().focused_modal_input = engine.input_state();
        }
        if let Some(state) = editor.component_view(self.region.id()) {
            self.history = state.history.clone();
        }
        self.engine = Some(engine);
        Some(result)
    }
}

// ---------------------------------------------------------------------------
// Trait impls — delegate to inner BaseViewport
// ---------------------------------------------------------------------------

impl Identified for EditRegion {
    fn id(&self) -> ViewId {
        self.region.id()
    }
}

impl Bounded for EditRegion {
    fn area(&self) -> Rect {
        self.region.area()
    }

    fn set_area(&mut self, area: Rect) {
        self.region.set_area(area);
    }
}

impl Focusable for EditRegion {
    fn is_focused(&self) -> bool {
        self.region.is_focused()
    }

    fn set_focused(&mut self, focused: bool) {
        self.region.set_focused(focused);
    }
}

impl Modal for EditRegion {
    fn mode(&self) -> Mode {
        self.mode
    }

    fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }
}

impl NavigableViewport<Document> for EditRegion {
    fn text_area_width(&self, _doc: &Document) -> u16 {
        self.area().width
    }

    fn text_annotations<'a>(
        &self,
        _doc: &'a Document,
    ) -> helix_core::text_annotations::TextAnnotations<'a> {
        helix_core::text_annotations::TextAnnotations::default()
    }
}

impl TextViewport<Document> for EditRegion {
    fn text_area(&self, _doc: &Document) -> Rect {
        self.area()
    }

    fn view_offset(&self, doc: &Document) -> ViewPosition {
        doc.view_offset(self.id())
    }

    fn set_view_offset(&self, doc: &mut Document, pos: ViewPosition) {
        doc.set_view_offset(self.id(), pos);
    }
}

impl HistoryViewport<Document> for EditRegion {
    fn apply_history_transaction(
        &mut self,
        transaction: &helix_core::Transaction,
        doc: &mut Document,
    ) {
        self.history.apply(transaction, doc);
    }

    fn sync_changes(&mut self, doc: &mut Document) {
        self.history.sync_changes(doc);
    }
}

impl Jumpable<Document> for EditRegion {
    fn push_jump(&mut self, doc: &mut Document) {
        let view_id = self.id();
        doc.append_changes_to_history(self);
        self.history
            .jumps
            .push((doc.id(), doc.selection(view_id).clone()));
    }
}
