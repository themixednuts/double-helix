//! Headless integration tests for the editing engines.
//!
//! These tests construct an Editor + Engine without any terminal, compositor,
//! or async UI handlers — proving the engine is frontend-independent.

use std::sync::Arc;

use arc_swap::ArcSwap;
use helix_view::editor::Config;
use helix_view::engine::{
    ActionId, CommandToken, EditingEngine, EngineResult, KeymapLookup, KeymapQuery, MotionId,
    OperatorId,
};
use helix_view::graphics::Rect;
use helix_view::input::KeyEvent;
use helix_view::keyboard::{KeyCode, KeyModifiers};
use helix_view::Editor;

use helix_modal::helix::HelixEngine;
use helix_modal::populate::build_registry;
use helix_modal::vim::VimEngine;

// ─── Test helpers ────────────────────────────────────────────────────

/// Minimal keymap for testing — maps keys directly to typed commands.
struct TestKeymap {
    mappings: Vec<(helix_view::document::Mode, KeyEvent, CommandToken)>,
}

impl TestKeymap {
    fn new() -> Self {
        use helix_view::document::Mode;

        let mut mappings = Vec::new();
        let n = Mode::Normal;

        // Movement
        mappings.push((
            n,
            char_key('h'),
            CommandToken::Motion(MotionId::new("move_char_left")),
        ));
        mappings.push((
            n,
            char_key('l'),
            CommandToken::Motion(MotionId::new("move_char_right")),
        ));
        mappings.push((
            n,
            char_key('j'),
            CommandToken::Motion(MotionId::new("move_line_down")),
        ));
        mappings.push((
            n,
            char_key('k'),
            CommandToken::Motion(MotionId::new("move_line_up")),
        ));
        mappings.push((
            n,
            char_key('w'),
            CommandToken::Motion(MotionId::new("move_next_word_start")),
        ));
        mappings.push((
            n,
            char_key('b'),
            CommandToken::Motion(MotionId::new("move_prev_word_start")),
        ));
        mappings.push((
            n,
            char_key('e'),
            CommandToken::Motion(MotionId::new("move_next_word_end")),
        ));

        // Operators (for Vim engine)
        mappings.push((
            n,
            char_key('d'),
            CommandToken::Operator(OperatorId::new("delete_selection")),
        ));
        mappings.push((
            n,
            char_key('y'),
            CommandToken::Operator(OperatorId::new("yank")),
        ));
        mappings.push((
            n,
            char_key('c'),
            CommandToken::Operator(OperatorId::new("change_selection")),
        ));

        // Actions
        mappings.push((
            n,
            char_key('x'),
            CommandToken::Operator(OperatorId::new("delete_selection")),
        ));
        mappings.push((
            n,
            char_key('u'),
            CommandToken::Action(ActionId::new("undo")),
        ));

        Self { mappings }
    }

    /// Look up a key and return the KeymapLookup.
    fn lookup(&self, mode: helix_view::document::Mode, key: KeyEvent) -> KeymapLookup {
        for (m, k, command) in &self.mappings {
            if *m == mode && *k == key {
                return KeymapLookup::Matched(*command);
            }
        }
        KeymapLookup::NotFound
    }
}

impl KeymapQuery for TestKeymap {
    fn contains_key(&self, mode: helix_view::document::Mode, key: KeyEvent) -> bool {
        self.mappings
            .iter()
            .any(|(m, k, _)| *m == mode && *k == key)
    }

    fn pending(&self) -> &[KeyEvent] {
        &[]
    }

    fn has_sticky(&self) -> bool {
        false
    }

    fn sticky_infobox(&self) -> Option<helix_view::info::Info> {
        None
    }

    fn clear_sticky(&mut self) {}
}

/// Feed a key through the engine using pre_resolve + process_lookup.
fn feed_key(
    engine: &mut dyn EditingEngine,
    editor: &mut Editor,
    keymaps: &mut TestKeymap,
    key: KeyEvent,
) -> EngineResult {
    let view_id = editor.tree.focus;
    let doc_id = editor.tree.get(view_id).doc;

    // Step 1: pre_resolve (count, register, dot-repeat, escape)
    if let Some(result) = engine.pre_resolve(editor, view_id, doc_id, keymaps, key) {
        return result;
    }

    // Step 2: resolve keymap
    let mode = editor.mode();
    let lookup = keymaps.lookup(mode, key);

    // Step 3: process lookup
    engine.process_lookup(editor, view_id, doc_id, keymaps, key, lookup)
}

fn char_key(ch: char) -> KeyEvent {
    KeyEvent {
        code: KeyCode::Char(ch),
        modifiers: KeyModifiers::empty(),
    }
}

/// Create a minimal Editor for testing (requires tokio runtime for Handlers).
fn test_editor() -> Editor {
    let theme_loader = helix_view::theme::Loader::new(helix_loader::runtime_dirs());
    let syn_loader = helix_core::config::default_lang_loader();
    let config = Config::default();
    let config = Arc::new(ArcSwap::from_pointee(config));

    let handlers = helix_view::handlers::Handlers::dummy();

    Editor::new(
        Rect::new(0, 0, 80, 24),
        Arc::new(theme_loader),
        Arc::new(ArcSwap::from_pointee(syn_loader)),
        Arc::new(arc_swap::access::Map::new(config, |c: &Config| c)),
        helix_runtime::test::runtime(),
        handlers,
    )
}

fn build_helix_engine() -> Box<dyn EditingEngine> {
    let registry = Arc::new(build_registry());
    Box::new(HelixEngine::new(registry))
}

fn build_vim_engine() -> Box<dyn EditingEngine> {
    let registry = Arc::new(build_registry());
    Box::new(VimEngine::new(registry))
}

/// Open a scratch buffer with content and place cursor at position 0.
fn editor_with_content(editor: &mut Editor, content: &str) {
    let doc_id = editor.new_file(helix_view::editor::Action::VerticalSplit);
    let view_id = editor.tree.focus;
    let doc = editor.document_mut(doc_id).unwrap();
    let tx = helix_core::Transaction::change(
        doc.text(),
        [(0, doc.text().len_chars(), Some(content.into()))].into_iter(),
    );
    doc.apply(&tx, view_id);
    doc.set_selection(view_id, helix_core::Selection::point(0));
}

/// Get the cursor position (primary selection head) in the current document.
fn cursor_pos(editor: &Editor) -> usize {
    let (view_id, doc) = helix_view::focused_ref!(editor);
    let text = doc.text().slice(..);
    doc.selection(view_id).primary().cursor(text)
}

/// Get the current document text.
fn doc_text(editor: &Editor) -> String {
    let (_, doc) = helix_view::focused_ref!(editor);
    doc.text().to_string()
}

// ─── Tests ───────────────────────────────────────────────────────────

#[tokio::test]
async fn helix_engine_motion_moves_cursor() {
    let mut editor = test_editor();
    let mut engine = build_helix_engine();
    let mut keymaps = TestKeymap::new();
    editor_with_content(&mut editor, "hello world\n");

    for _ in 0..3 {
        let result = feed_key(&mut *engine, &mut editor, &mut keymaps, char_key('l'));
        assert!(matches!(result, EngineResult::Executed));
    }

    assert_eq!(cursor_pos(&editor), 3);
}

#[tokio::test]
async fn helix_engine_count_then_motion() {
    let mut editor = test_editor();
    let mut engine = build_helix_engine();
    let mut keymaps = TestKeymap::new();
    editor_with_content(&mut editor, "hello world\n");

    let result = feed_key(&mut *engine, &mut editor, &mut keymaps, char_key('3'));
    assert!(matches!(result, EngineResult::Pending));

    let result = feed_key(&mut *engine, &mut editor, &mut keymaps, char_key('l'));
    assert!(matches!(result, EngineResult::Executed));

    assert_eq!(cursor_pos(&editor), 3);
}

#[tokio::test]
async fn helix_engine_unbound_key() {
    let mut editor = test_editor();
    let mut engine = build_helix_engine();
    let mut keymaps = TestKeymap::new();
    editor_with_content(&mut editor, "hello\n");

    let result = feed_key(&mut *engine, &mut editor, &mut keymaps, char_key('z'));
    assert!(matches!(result, EngineResult::Unbound));
}

#[tokio::test]
async fn vim_engine_operator_pending_then_motion() {
    let mut editor = test_editor();
    let mut engine = build_vim_engine();
    let mut keymaps = TestKeymap::new();
    editor_with_content(&mut editor, "hello world\n");

    let result = feed_key(&mut *engine, &mut editor, &mut keymaps, char_key('d'));
    assert!(matches!(result, EngineResult::Pending));

    let result = feed_key(&mut *engine, &mut editor, &mut keymaps, char_key('w'));
    assert!(matches!(result, EngineResult::Executed));

    assert_eq!(doc_text(&editor), "world\n");
}

#[tokio::test]
async fn vim_engine_doubled_operator_linewise() {
    let mut editor = test_editor();
    let mut engine = build_vim_engine();
    let mut keymaps = TestKeymap::new();
    editor_with_content(&mut editor, "line one\nline two\nline three\n");

    let result = feed_key(&mut *engine, &mut editor, &mut keymaps, char_key('d'));
    assert!(matches!(result, EngineResult::Pending));

    let result = feed_key(&mut *engine, &mut editor, &mut keymaps, char_key('d'));
    assert!(matches!(result, EngineResult::Executed));

    assert_eq!(doc_text(&editor), "line two\nline three\n");
}

#[tokio::test]
async fn vim_engine_count_multiplication() {
    let mut editor = test_editor();
    let mut engine = build_vim_engine();
    let mut keymaps = TestKeymap::new();
    editor_with_content(&mut editor, "abcdefghij\n");

    let result = feed_key(&mut *engine, &mut editor, &mut keymaps, char_key('2'));
    assert!(matches!(result, EngineResult::Pending));

    let result = feed_key(&mut *engine, &mut editor, &mut keymaps, char_key('l'));
    assert!(matches!(result, EngineResult::Executed));

    assert_eq!(cursor_pos(&editor), 2);
}

#[tokio::test]
async fn engine_mode_names() {
    let helix = build_helix_engine();
    assert_eq!(helix.name(), "helix");

    let vim = build_vim_engine();
    assert_eq!(vim.name(), "vim");
}

#[tokio::test]
async fn registry_has_commands() {
    let registry = build_registry();
    assert!(
        registry.len() > 100,
        "registry should have 100+ commands, got {}",
        registry.len()
    );
    assert!(registry.motion(MotionId::new("move_char_left")).is_some());
    assert!(registry
        .operator(OperatorId::new("delete_selection"))
        .is_some());
    assert!(registry.operator(OperatorId::new("yank")).is_some());
    assert!(registry.action(ActionId::new("undo")).is_some());
    assert!(registry
        .char_pending(helix_view::engine::CharPendingId::new(
            "select_textobject_inside_surrounding_pair",
        ))
        .is_some());
}
