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
use crate::input::KeyEvent;
use crate::keymap::ModalKeymaps;
use crate::traits::{
    Bounded, Focusable, HistoryViewport, Identified, Jumpable, Modal, NavigableViewport,
    TextViewport,
};
use crate::view::ViewPosition;
use crate::{Document, DocumentId, Editor, ViewId};

/// How a fresh entry into Insert mode should position the cursor.
///
/// One variant per Helix entry command, with the exact same cursor
/// semantics — so any UI surface embedding an [`EditRegion`] (the file
/// explorer's label rename, the assistant input, future cmdline migration,
/// …) reuses the editor's `i` / `a` / `I` / `A` behavior without
/// reimplementing the selection transforms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsertEntry {
    /// Helix `i` — cursor at the start of the current selection
    /// (`range.from()`). For a point selection, the cursor doesn't move.
    AtCurrent,
    /// Helix `a` — cursor one grapheme past the current selection's end
    /// (`next_grapheme_boundary(range.to())`). For a point selection,
    /// this advances the cursor by exactly one grapheme.
    Append,
    /// Helix `I` — cursor at the start of the line containing the cursor.
    AtLineStart,
    /// Helix `A` — cursor at the end of the line containing the cursor
    /// (before the newline, if any).
    AtLineEnd,
}

impl InsertEntry {
    /// The Helix engine command name corresponding to this entry. Used
    /// for insert recording (so `.` repeat / macros can replay the entry
    /// later) and for telemetry.
    pub fn engine_command(self) -> std::borrow::Cow<'static, str> {
        match self {
            InsertEntry::AtCurrent | InsertEntry::AtLineStart => "insert_mode".into(),
            InsertEntry::Append | InsertEntry::AtLineEnd => "append_mode".into(),
        }
    }
}

/// Per-call policy that controls how an [`EditRegion`] interprets keys
/// which typically have host-specific meaning (Enter, Esc, newlines, …).
///
/// The point of this struct is to make each call site declare its
/// policy *once*, in one place, instead of forking the dispatch logic
/// across surfaces. The file explorer's label rename, the assistant
/// input, the (future) cmdline, and the (future) picker filter all
/// share the same dispatch path — they just hand the region a different
/// `HostPolicy` and the region does the right thing.
///
/// Don't construct this with named-field syntax in hot paths; the helpers
/// ([`HostPolicy::single_line_commit`], [`HostPolicy::multiline`]) describe
/// the common shapes and are self-documenting at the call site.
#[derive(Clone, Copy, Debug, Default)]
pub struct HostPolicy {
    /// When true, newline characters inserted by the engine are dropped,
    /// and Enter in Insert mode signals submit instead of inserting a
    /// line break. Single-line hosts (cmdline, picker filter, file
    /// explorer label) want this; multi-line hosts (assistant) don't.
    pub single_line: bool,

    /// When true, Esc in Insert mode signals submit immediately instead
    /// of dropping to Normal mode. The default — and what
    /// [`HostPolicy::single_line_commit`] chooses — is `false`, so Esc
    /// drops to Normal and the user can use word motions (`w`/`b`/`e`),
    /// text-object operators (`d`/`c`/`y`), in-buffer undo, etc. before
    /// committing.
    pub esc_commits_in_insert: bool,

    /// When true, Esc in Normal mode signals submit. Used for
    /// single-line hosts so the "second Esc" commits without ever
    /// showing a popup or requiring a special key. Combined with
    /// `esc_commits_in_insert: false`, this gives the user a clean
    /// Esc → motions → Esc workflow.
    pub esc_commits_in_normal: bool,

    /// When true, Enter in Normal mode signals submit. In Insert mode
    /// Enter's behavior is governed by [`HostPolicy::single_line`]: a
    /// single-line host treats Enter-in-Insert as submit; a multi-line
    /// host lets it insert a newline. Single-line surfaces typically
    /// want this `true` so Enter from any mode commits.
    pub enter_submits_in_normal: bool,
}

impl HostPolicy {
    /// Policy for a single-line surface that supports the full editor
    /// modal vocabulary: file explorer label rename, future cmdline,
    /// future picker filter.
    ///
    /// Workflow:
    /// - Enter inline edit (host calls
    ///   [`Self::enter_insert_at`](EditRegion::enter_insert_at)).
    /// - Type characters in Insert mode.
    /// - Press Enter to commit immediately, OR
    /// - Press Esc to drop to Normal mode and use word motions /
    ///   operators / undo, then press Enter or Esc again to commit.
    /// - Ctrl-C cancels at any point.
    ///
    /// This deliberately replaces the older "single Esc commits"
    /// behavior because it was blocking access to Normal mode
    /// motions inside the label edit. The cost is one extra
    /// keystroke for the Esc-then-commit path; the benefit is the
    /// full editor command set (`w`/`b`/`e`/`d`/`c`/`u`/`U`/`gg`/`G`/…)
    /// inside the inline rename buffer.
    pub const fn single_line_commit() -> Self {
        Self {
            single_line: true,
            esc_commits_in_insert: false,
            esc_commits_in_normal: true,
            enter_submits_in_normal: true,
        }
    }

    /// Policy for a multi-line surface that submits on Enter from Normal
    /// mode but otherwise behaves like a real editor buffer: the
    /// assistant input.
    pub const fn multiline() -> Self {
        Self {
            single_line: false,
            esc_commits_in_insert: false,
            esc_commits_in_normal: false,
            enter_submits_in_normal: true,
        }
    }
}

/// The outcome of dispatching a single key through an [`EditRegion`].
///
/// Hosts inspect this value to know whether the key was handled inside
/// the region, whether the user is asking to submit or cancel, or whether
/// the key should bubble back up to the parent dispatcher (the compositor,
/// the explorer panel's Normal-mode action handler, etc.).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DispatchSignal {
    /// Key was consumed — by the modal engine, by [`HostPolicy`], or by
    /// the entry-into-Insert helpers. The host should call its
    /// post-dispatch sync hooks (e.g. updating a cached display string)
    /// and otherwise do nothing.
    Consumed,
    /// The user pressed a "submit" key for this policy (Enter in single-
    /// line Insert, Esc with `esc_commits`, etc.). The host should read
    /// the buffer's contents and act on them — e.g. perform the rename,
    /// send the prompt, run the command. The buffer is NOT cleared by
    /// `dispatch`; the host owns when to clear or take the text.
    Submit,
    /// The user pressed a "cancel" key (Ctrl-C always cancels). The host
    /// should discard whatever state was being edited.
    Cancel,
    /// The key was not handled. The host should bubble it up to its
    /// parent — for the file explorer that usually means "swallow" since
    /// the explorer eats keys while editing; for the assistant it means
    /// "let the global editor handle it" (e.g. `:` opens cmdline).
    Bubble,
}

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
    ///
    /// This is the low-level primitive. Most callers should use
    /// [`Self::enter_insert_at`] instead, which also moves the cursor to
    /// match Helix's `i` / `a` / `I` / `A` semantics — keeping every
    /// surface that embeds an [`EditRegion`] in lockstep with the editor.
    pub fn enter_insert_mode(&mut self, entry_command: std::borrow::Cow<'static, str>) {
        self.mode = Mode::Insert;
        if let Some(engine) = &mut self.engine {
            engine.begin_insert_recording(entry_command);
        }
    }

    /// Enter insert mode and move the cursor to match Helix's editor
    /// semantics for the given entry kind. Mirrors `commands::insert_mode`,
    /// `commands::append_mode`, `commands::insert_at_line_start`, and
    /// `commands::insert_at_line_end` from `helix-term/src/commands.rs`,
    /// but operates on this region's own document instead of the editor's
    /// globally-focused one.
    ///
    /// This is the canonical entry point — hosts should call this from
    /// their Frontend-intent dispatch (`insert_mode` / `append_mode` /
    /// `insert_at_line_start` / `insert_at_line_end`) so the editor and
    /// every embedded `EditRegion` stay in agreement about what `a` means.
    /// Folding the selection transform in here is what makes the abstraction
    /// drift-proof: there's no longer a per-host hand-rolled match that can
    /// silently collapse two distinct entries into the same cursor position.
    pub fn enter_insert_at(&mut self, editor: &mut Editor, entry: InsertEntry) {
        self.enter_insert_mode(entry.engine_command());
        let Some(doc_id) = self.doc_id else { return };
        let view_id = self.region.id();
        let Some(doc) = editor.component_docs.get_mut(&doc_id) else {
            return;
        };
        let text = doc.text().slice(..);
        let selection = doc.selection(view_id).clone().transform(|range| {
            // These transforms intentionally mirror Helix's editor commands
            // *byte for byte* — `commands::insert_mode`, `commands::append_mode`,
            // `commands::insert_at_line_start`, and `commands::insert_at_line_end`
            // in `helix-term/src/commands.rs`. Don't collapse to a point with
            // `Range::new(new, new)` — `Selection::ensure_invariants` will then
            // call `min_width_1`, extending the head one grapheme past where you
            // wanted it, and your `a` will silently land *two* graphemes past
            // the cursor instead of one. The editor avoids this by keeping
            // the range's structural shape (anchor/head pair) and letting the
            // resulting `Range::cursor()` derive the visual position.
            match entry {
                // `i`: flip anchor↔head so the cursor sits at the start of
                // the original range. For a point range this is a no-op.
                InsertEntry::AtCurrent => helix_core::Range::new(range.to(), range.from()),
                // `a`: extend the head one grapheme past `range.to()`. The
                // resulting forward range has `cursor() == old_to`, which
                // is one past `range.cursor()` for a point selection.
                InsertEntry::Append => helix_core::Range::new(
                    range.from(),
                    helix_core::graphemes::next_grapheme_boundary(text, range.to()),
                ),
                // `I`: cursor at the start of the cursor's line. Editor's
                // `insert_at_line_start` also handles auto-indent for empty
                // lines; that branch is editor-only and irrelevant for the
                // single-line surfaces that embed an `EditRegion`.
                InsertEntry::AtLineStart => {
                    let line = text.char_to_line(range.cursor(text));
                    let pos = text.line_to_char(line);
                    helix_core::Range::new(pos, pos)
                }
                // `A`: cursor at the end of the cursor's line (before the
                // newline, if any).
                InsertEntry::AtLineEnd => {
                    let line = text.char_to_line(range.cursor(text));
                    let pos = helix_core::line_ending::line_end_char_index(&text, line);
                    helix_core::Range::new(pos, pos)
                }
            }
        });
        doc.set_selection(view_id, selection);
    }

    /// Exit insert mode: set the region mode back to normal and finalize
    /// the engine's insert recording.
    pub fn exit_insert_mode(&mut self) {
        self.mode = Mode::Normal;
        if let Some(engine) = &mut self.engine {
            engine.end_insert_recording();
        }
    }

    /// Insert a single character at every cursor in the region's backing
    /// document. Used internally by [`Self::dispatch`] when the engine
    /// emits [`crate::engine::EngineResult::InsertChar`], and exposed for
    /// hosts that need to inject characters directly (e.g. paste handlers,
    /// the assistant's Tab-inserts-tab branch).
    pub fn insert_char(&mut self, editor: &mut Editor, ch: char) {
        let Some(doc_id) = self.doc_id else { return };
        let view_id = self.region.id();
        let Some(doc) = editor.component_docs.get_mut(&doc_id) else {
            return;
        };
        let text = doc.text();
        let selection = doc.selection(view_id).clone();
        let cursors = selection.cursors(text.slice(..));
        let mut tendril = helix_core::Tendril::new();
        tendril.push(ch);
        let transaction = helix_core::Transaction::insert(text, &cursors, tendril);
        doc.apply(&transaction, view_id);
    }

    fn insert_key_chars(&mut self, editor: &mut Editor, keys: &[KeyEvent], policy: HostPolicy) {
        for key in keys {
            if let Some(ch) = key.char() {
                if policy.single_line && (ch == '\n' || ch == '\r') {
                    continue;
                }
                self.insert_char(editor, ch);
            }
        }
    }

    /// Read the current buffer text. Returns `None` if the region has
    /// not been initialized yet. Allocates — for hot paths, prefer
    /// accessing the document directly via [`Self::document`].
    pub fn text(&self, editor: &Editor) -> Option<String> {
        self.document(editor).map(|doc| doc.text().to_string())
    }

    /// Replace the buffer with `text` and place a point cursor at `cursor`
    /// (clamped to the new text length).
    ///
    /// Lazy-initializes the region if it hasn't been touched yet. Hosts
    /// call this when they "start" an edit — the file explorer label
    /// rename uses it to seed the buffer with the row's current name
    /// before entering Insert mode.
    pub fn set_text(&mut self, editor: &mut Editor, text: &str, cursor: usize) {
        self.ensure_init(editor);
        let Some(doc_id) = self.doc_id else { return };
        let view_id = self.region.id();
        let Some(doc) = editor.component_docs.get_mut(&doc_id) else {
            return;
        };
        let len = doc.text().len_chars();
        let transaction =
            helix_core::Transaction::change(doc.text(), [(0, len, Some(text.into()))].into_iter());
        doc.apply(&transaction, view_id);
        let cursor = cursor.min(text.chars().count());
        doc.set_selection(view_id, helix_core::Selection::point(cursor));
    }

    /// Clear the buffer's contents and reset to Normal mode. Idempotent
    /// — safe to call before the region has been initialized.
    pub fn clear(&mut self, editor: &mut Editor) {
        if self.doc_id.is_some() {
            self.set_text(editor, "", 0);
        }
        if self.mode == Mode::Insert {
            self.exit_insert_mode();
        }
    }

    /// Dispatch a key through the region using a [`HostPolicy`].
    ///
    /// This is the unified entry point — every host that embeds an
    /// `EditRegion` should funnel keys through here (after intercepting
    /// any surface-specific keys it owns, like the assistant's Tab /
    /// Shift-Tab handlers). The returned [`DispatchSignal`] tells the
    /// host whether to submit, cancel, treat as consumed, or bubble.
    ///
    /// What this method handles, so hosts don't have to:
    /// - `i` / `a` / `I` / `A` in Normal mode → enter Insert via
    ///   [`Self::enter_insert_at`] with the right cursor placement
    /// - Esc in Insert mode → either commit (per policy) or drop to Normal
    /// - Enter in Insert mode with `single_line` → commit
    /// - Enter in Normal mode with `enter_submits_in_normal` → commit
    /// - Ctrl-C anywhere → cancel
    /// - Engine-emitted [`crate::engine::EngineResult::InsertChar`] →
    ///   inserts the char into the buffer; newlines filtered when
    ///   `single_line` is set
    /// - Dot-repeat / cancelled-insert key replay
    ///
    /// What hosts still own: surface-specific pre-intercepts (Tab/Shift-Tab
    /// for assistant, Up/Down for future cmdline history) and the actual
    /// submit/cancel side effects (rename the file, send the prompt, …).
    pub fn dispatch(
        &mut self,
        editor: &mut Editor,
        key: crate::input::KeyEvent,
        policy: HostPolicy,
    ) -> DispatchSignal {
        use crate::input::{KeyCode, KeyModifiers};

        // --- Pre-engine policy interception ----------------------------------
        // Ctrl-C is the universal cancel; check it first so a Ctrl-C in
        // either mode bails cleanly.
        if matches!(key.code, KeyCode::Char('c')) && key.modifiers == KeyModifiers::CONTROL {
            return DispatchSignal::Cancel;
        }

        match self.mode() {
            Mode::Normal => {
                if key.modifiers.is_empty() {
                    let entry = match key.code {
                        KeyCode::Char('i') => Some(InsertEntry::AtCurrent),
                        KeyCode::Char('a') => Some(InsertEntry::Append),
                        KeyCode::Char('I') => Some(InsertEntry::AtLineStart),
                        KeyCode::Char('A') => Some(InsertEntry::AtLineEnd),
                        _ => None,
                    };
                    if let Some(entry) = entry {
                        self.enter_insert_at(editor, entry);
                        return DispatchSignal::Consumed;
                    }
                    if policy.enter_submits_in_normal && matches!(key.code, KeyCode::Enter) {
                        return DispatchSignal::Submit;
                    }
                    if policy.esc_commits_in_normal && matches!(key.code, KeyCode::Esc) {
                        return DispatchSignal::Submit;
                    }
                }
            }
            Mode::Insert => {
                if key.modifiers.is_empty() {
                    if matches!(key.code, KeyCode::Esc) {
                        if policy.esc_commits_in_insert {
                            return DispatchSignal::Submit;
                        } else {
                            self.exit_insert_mode();
                            return DispatchSignal::Consumed;
                        }
                    }
                    if policy.single_line && matches!(key.code, KeyCode::Enter) {
                        return DispatchSignal::Submit;
                    }
                }
            }
            Mode::Select => {}
        }

        // --- Engine dispatch -------------------------------------------------
        let Some(result) = self.dispatch_key(editor, key) else {
            return DispatchSignal::Bubble;
        };

        use crate::engine::EngineResult;
        match result {
            EngineResult::Executed | EngineResult::Pending => DispatchSignal::Consumed,
            EngineResult::Unbound => DispatchSignal::Bubble,
            EngineResult::InsertChar(ch) => {
                if policy.single_line && (ch == '\n' || ch == '\r') {
                    // Swallow stray newlines on single-line surfaces. The
                    // engine emits an InsertChar('\n') when the user types
                    // a literal newline in Insert mode; we already convert
                    // the Enter key to Submit above, so the only way to get
                    // here is via paste of multi-line content or unusual
                    // input methods. Better to silently drop than to corrupt
                    // a single-line buffer with embedded newlines.
                    return DispatchSignal::Consumed;
                }
                self.insert_char(editor, ch);
                DispatchSignal::Consumed
            }
            EngineResult::CancelledInsert(keys) => {
                self.insert_key_chars(editor, &keys, policy);
                DispatchSignal::Consumed
            }
            EngineResult::ReplayInsert { keys, .. } => {
                self.insert_key_chars(editor, &keys, policy);
                DispatchSignal::Consumed
            }
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
        key: KeyEvent,
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

#[cfg(test)]
mod tests {
    //! These tests pin down the contract that every UI surface embedding
    //! an [`EditRegion`] inherits: `i` / `a` / `I` / `A` place the cursor
    //! at the *same* positions they would in the main editor. If you
    //! change the selection-transform logic in [`EditRegion::enter_insert_at`],
    //! these tests have to flip too — and so does the main editor's
    //! `commands::insert_mode` / `append_mode` / `insert_at_line_start` /
    //! `insert_at_line_end`, which is the point.
    use super::*;
    use crate::handlers::Handlers;
    use crate::theme;
    use crate::view::View;
    use arc_swap::ArcSwap;
    use helix_core::{syntax, Selection};
    use std::sync::Arc;

    fn test_editor(runtime: helix_runtime::Runtime) -> Editor {
        let theme_loader = Arc::new(theme::Loader::new(&[]));
        let syn_loader = Arc::new(ArcSwap::from_pointee(syntax::Loader::default()));
        let config = Arc::new(ArcSwap::from_pointee(crate::editor::Config::default()));
        let handlers = Handlers::dummy();
        let mut editor = Editor::new(
            crate::graphics::Rect::new(0, 0, 80, 24),
            theme_loader,
            syn_loader,
            config,
            runtime,
            handlers,
        );
        // Need a regular view + doc so the editor invariants hold; the
        // EditRegion creates its own component_doc on init.
        let doc_id = editor.new_document(Document::default(
            editor.config.clone(),
            editor.syn_loader.clone(),
        ));
        let mut view = View::new(doc_id, editor.config().gutters.clone());
        editor.bind_view_redraw(&mut view);
        let view_id = editor.tree.insert(view);
        let _ = editor.track_tree_surface(view_id);
        let doc = crate::doc_mut!(editor, &doc_id);
        doc.ensure_view_init(view_id);
        editor
    }

    /// Seed the EditRegion's backing document with `text` and a point
    /// selection at `cursor`.
    fn seed(region: &mut EditRegion, editor: &mut Editor, text: &str, cursor: usize) {
        region.ensure_init(editor);
        let view_id = region.view_id();
        let doc_id = region.doc_id().expect("ensure_init populates doc_id");
        let doc = editor
            .component_docs
            .get_mut(&doc_id)
            .expect("backing doc exists");
        // Replace the (initially empty) rope with `text`.
        let len = doc.text().len_chars();
        let transaction =
            helix_core::Transaction::change(doc.text(), [(0, len, Some(text.into()))].into_iter());
        doc.apply(&transaction, view_id);
        doc.set_selection(view_id, Selection::point(cursor));
    }

    fn cursor(region: &EditRegion, editor: &Editor) -> usize {
        let doc = region.document(editor).expect("doc exists");
        let text = doc.text().slice(..);
        doc.selection(region.view_id()).primary().cursor(text)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn insert_entry_at_current_leaves_point_cursor_in_place() {
        let runtime = helix_runtime::test::runtime();
        let mut editor = test_editor(runtime);
        let mut region = EditRegion::default();
        seed(&mut region, &mut editor, "alpha.rs", 3);

        region.enter_insert_at(&mut editor, InsertEntry::AtCurrent);

        assert_eq!(region.mode(), Mode::Insert);
        assert_eq!(cursor(&region, &editor), 3);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn insert_entry_append_advances_point_cursor_one_grapheme() {
        let runtime = helix_runtime::test::runtime();
        let mut editor = test_editor(runtime);
        let mut region = EditRegion::default();
        seed(&mut region, &mut editor, "alpha.rs", 3);

        region.enter_insert_at(&mut editor, InsertEntry::Append);

        assert_eq!(region.mode(), Mode::Insert);
        // `a` from cursor 3 on "alpha.rs" lands at 4 — exactly one
        // grapheme past, matching the editor's `append_mode`.
        assert_eq!(cursor(&region, &editor), 4);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn insert_entry_append_clamps_at_end_of_buffer() {
        let runtime = helix_runtime::test::runtime();
        let mut editor = test_editor(runtime);
        let mut region = EditRegion::default();
        let len = "alpha.rs".chars().count();
        seed(&mut region, &mut editor, "alpha.rs", len);

        region.enter_insert_at(&mut editor, InsertEntry::Append);

        // `next_grapheme_boundary` at the end of the rope returns the
        // end position — no panic, no wrap, just clamp.
        assert_eq!(cursor(&region, &editor), len);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn insert_entry_at_line_start_lands_at_column_zero() {
        let runtime = helix_runtime::test::runtime();
        let mut editor = test_editor(runtime);
        let mut region = EditRegion::default();
        seed(&mut region, &mut editor, "first\nsecond", 8); // cursor in "second"

        region.enter_insert_at(&mut editor, InsertEntry::AtLineStart);

        // First char of "second" = 6 ("first\n" = 6 chars).
        assert_eq!(cursor(&region, &editor), 6);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn insert_entry_at_line_end_lands_before_newline() {
        let runtime = helix_runtime::test::runtime();
        let mut editor = test_editor(runtime);
        let mut region = EditRegion::default();
        seed(&mut region, &mut editor, "first\nsecond", 1); // cursor in "first"

        region.enter_insert_at(&mut editor, InsertEntry::AtLineEnd);

        // End of "first" = 5 (right before '\n').
        assert_eq!(cursor(&region, &editor), 5);
    }

    /// The contract test: `a` lands strictly one position past `i` for
    /// any non-end point selection. This is the property that, when
    /// violated, lets `a` masquerade as `i` — exactly the bug we just
    /// fixed in the file explorer.
    #[tokio::test(flavor = "current_thread")]
    async fn append_lands_one_past_at_current_for_point_selection() {
        for cursor_pos in 0..7 {
            let runtime = helix_runtime::test::runtime();
            let mut editor = test_editor(runtime);

            let mut at_current = EditRegion::default();
            seed(&mut at_current, &mut editor, "alpha.rs", cursor_pos);
            at_current.enter_insert_at(&mut editor, InsertEntry::AtCurrent);
            let after_i = cursor(&at_current, &editor);

            let mut appended = EditRegion::default();
            seed(&mut appended, &mut editor, "alpha.rs", cursor_pos);
            appended.enter_insert_at(&mut editor, InsertEntry::Append);
            let after_a = cursor(&appended, &editor);

            assert_eq!(
                after_a,
                after_i + 1,
                "Append should land one grapheme past AtCurrent (start cursor={cursor_pos})",
            );
        }
    }

    /// The unified dispatch path treats `i` and `a` in Normal mode as
    /// entry into Insert, with the same cursor semantics as the
    /// standalone [`EditRegion::enter_insert_at`]. This is the test
    /// that pins the host-facing contract — every UI surface that funnels
    /// keys through [`EditRegion::dispatch`] gets these semantics
    /// automatically and can't accidentally fork them.
    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_i_a_enter_insert_with_correct_cursor() {
        use crate::input::{KeyCode, KeyEvent, KeyModifiers};

        let runtime = helix_runtime::test::runtime();
        let mut editor = test_editor(runtime);

        let mut for_i = EditRegion::default();
        seed(&mut for_i, &mut editor, "alpha.rs", 3);
        let policy = HostPolicy::single_line_commit();
        let signal = for_i.dispatch(
            &mut editor,
            KeyEvent {
                code: KeyCode::Char('i'),
                modifiers: KeyModifiers::NONE,
            },
            policy,
        );
        assert_eq!(signal, DispatchSignal::Consumed);
        assert_eq!(for_i.mode(), Mode::Insert);
        assert_eq!(cursor(&for_i, &editor), 3);

        let mut for_a = EditRegion::default();
        seed(&mut for_a, &mut editor, "alpha.rs", 3);
        let signal = for_a.dispatch(
            &mut editor,
            KeyEvent {
                code: KeyCode::Char('a'),
                modifiers: KeyModifiers::NONE,
            },
            policy,
        );
        assert_eq!(signal, DispatchSignal::Consumed);
        assert_eq!(for_a.mode(), Mode::Insert);
        assert_eq!(cursor(&for_a, &editor), 4);
    }

    /// Single-line-commit policy: Esc and Enter in Insert mode both
    /// signal submit. Mirrors the file explorer's "Esc saves; no double
    /// press needed" UX requirement.
    /// Under single-line-commit, Enter in Insert mode submits
    /// immediately (one-key save from typing), and Esc in Normal mode
    /// also submits. Esc in Insert mode does NOT submit — it drops to
    /// Normal so the user can use motions. The "Esc, Esc" sequence is
    /// the two-key save path; the "type, Enter" sequence is the one-key
    /// path. This trade replaced the old "Esc in Insert commits"
    /// behavior so word motions / operators are reachable inside the
    /// edit without ever leaving the buffer.
    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_single_line_commit_paths() {
        use crate::input::{KeyCode, KeyEvent, KeyModifiers};

        let policy = HostPolicy::single_line_commit();

        // Path 1: Enter from Insert → Submit (one keystroke).
        {
            let runtime = helix_runtime::test::runtime();
            let mut editor = test_editor(runtime);
            let mut region = EditRegion::default();
            seed(&mut region, &mut editor, "alpha.rs", 0);
            region.enter_insert_at(&mut editor, InsertEntry::AtCurrent);
            let signal = region.dispatch(
                &mut editor,
                KeyEvent {
                    code: KeyCode::Enter,
                    modifiers: KeyModifiers::NONE,
                },
                policy,
            );
            assert_eq!(signal, DispatchSignal::Submit);
        }

        // Path 2: Esc from Insert → Consumed (drops to Normal). Then
        // a second Esc from Normal → Submit.
        {
            let runtime = helix_runtime::test::runtime();
            let mut editor = test_editor(runtime);
            let mut region = EditRegion::default();
            seed(&mut region, &mut editor, "alpha.rs", 0);
            region.enter_insert_at(&mut editor, InsertEntry::AtCurrent);
            let signal = region.dispatch(
                &mut editor,
                KeyEvent {
                    code: KeyCode::Esc,
                    modifiers: KeyModifiers::NONE,
                },
                policy,
            );
            assert_eq!(signal, DispatchSignal::Consumed);
            assert_eq!(region.mode(), Mode::Normal);

            let signal = region.dispatch(
                &mut editor,
                KeyEvent {
                    code: KeyCode::Esc,
                    modifiers: KeyModifiers::NONE,
                },
                policy,
            );
            assert_eq!(signal, DispatchSignal::Submit);
        }

        // Path 3: Enter from Normal → Submit (so the user can navigate
        // with motions, then commit without an extra Esc).
        {
            let runtime = helix_runtime::test::runtime();
            let mut editor = test_editor(runtime);
            let mut region = EditRegion::default();
            seed(&mut region, &mut editor, "alpha.rs", 0);
            region.enter_insert_at(&mut editor, InsertEntry::AtCurrent);
            region.exit_insert_mode();
            let signal = region.dispatch(
                &mut editor,
                KeyEvent {
                    code: KeyCode::Enter,
                    modifiers: KeyModifiers::NONE,
                },
                policy,
            );
            assert_eq!(signal, DispatchSignal::Submit);
        }
    }

    /// Multi-line policy: Esc returns to Normal mode (no submit). Enter
    /// in Insert mode is forwarded to the engine, which will emit
    /// `InsertChar('\n')` and the dispatcher inserts a newline.
    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_multiline_esc_drops_to_normal_does_not_submit() {
        use crate::input::{KeyCode, KeyEvent, KeyModifiers};

        let runtime = helix_runtime::test::runtime();
        let mut editor = test_editor(runtime);
        let mut region = EditRegion::default();
        seed(&mut region, &mut editor, "draft", 0);
        region.enter_insert_at(&mut editor, InsertEntry::AtCurrent);

        let signal = region.dispatch(
            &mut editor,
            KeyEvent {
                code: KeyCode::Esc,
                modifiers: KeyModifiers::NONE,
            },
            HostPolicy::multiline(),
        );

        assert_eq!(signal, DispatchSignal::Consumed);
        assert_eq!(region.mode(), Mode::Normal, "Esc should return to Normal");
    }

    /// Ctrl-C is the universal cancel — works in both Normal and Insert
    /// modes, under any policy. Hosts rely on this to give the user a
    /// always-available bail-out from any edit.
    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_ctrl_c_always_cancels() {
        use crate::input::{KeyCode, KeyEvent, KeyModifiers};

        for policy in [HostPolicy::single_line_commit(), HostPolicy::multiline()] {
            for entry_first in [false, true] {
                let runtime = helix_runtime::test::runtime();
                let mut editor = test_editor(runtime);
                let mut region = EditRegion::default();
                seed(&mut region, &mut editor, "anything", 0);
                if entry_first {
                    region.enter_insert_at(&mut editor, InsertEntry::AtCurrent);
                }

                let signal = region.dispatch(
                    &mut editor,
                    KeyEvent {
                        code: KeyCode::Char('c'),
                        modifiers: KeyModifiers::CONTROL,
                    },
                    policy,
                );
                assert_eq!(
                    signal,
                    DispatchSignal::Cancel,
                    "Ctrl-C should cancel under policy {policy:?} (insert_first={entry_first})",
                );
            }
        }
    }

    /// `set_text` + `clear` are the host-facing lifecycle calls. Seeding
    /// then clearing should leave the buffer empty and exit Insert mode
    /// if it was active.
    #[tokio::test(flavor = "current_thread")]
    async fn set_text_seeds_buffer_and_clear_resets() {
        let runtime = helix_runtime::test::runtime();
        let mut editor = test_editor(runtime);
        let mut region = EditRegion::default();

        region.set_text(&mut editor, "alpha.rs", 0);
        assert_eq!(
            region.text(&editor).as_deref(),
            Some("alpha.rs"),
            "set_text seeds the buffer",
        );
        region.enter_insert_at(&mut editor, InsertEntry::AtLineEnd);
        assert_eq!(region.mode(), Mode::Insert);

        region.clear(&mut editor);

        assert_eq!(region.text(&editor).as_deref(), Some(""));
        assert_eq!(region.mode(), Mode::Normal);
    }

    /// `insert_char` advances the cursor and edits the buffer. This is
    /// the path the engine uses when emitting `InsertChar`; pinning it
    /// down here guards against regressions in `Transaction::insert`'s
    /// selection mapping.
    #[tokio::test(flavor = "current_thread")]
    async fn insert_char_appends_and_advances_cursor() {
        let runtime = helix_runtime::test::runtime();
        let mut editor = test_editor(runtime);
        let mut region = EditRegion::default();
        seed(&mut region, &mut editor, "ab", 2);

        region.insert_char(&mut editor, 'c');

        assert_eq!(region.text(&editor).as_deref(), Some("abc"));
        assert_eq!(cursor(&region, &editor), 3);
    }

    #[test]
    fn insert_entry_engine_command_groups_i_and_capital_i() {
        // Helix records `i` and `I` under the same engine command so dot-
        // repeat replays them identically; same for `a` and `A`. This
        // matches the editor's `commands::insert_mode` /
        // `commands::insert_at_line_start` both starting with
        // `enter_insert_mode` and the `_` ID being `insert_mode`.
        assert_eq!(InsertEntry::AtCurrent.engine_command(), "insert_mode");
        assert_eq!(InsertEntry::AtLineStart.engine_command(), "insert_mode");
        assert_eq!(InsertEntry::Append.engine_command(), "append_mode");
        assert_eq!(InsertEntry::AtLineEnd.engine_command(), "append_mode");
    }
}
