//! Shared harness for rendering real UI Components in storybook stories.
//!
//! Stories use [`Stage`] to spin up a headless `Editor` with the story's theme
//! and config, seed it with realistic state (open documents, status messages,
//! pending diagnostics, etc.), build any `Component`, and render it through
//! the same `RenderContext` path the live editor uses.
//!
//! This is the single seam that keeps stories backed by real runtime UI:
//! whenever a story renders, the buffer it produces is what the runtime would
//! produce given the same state.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use crate::embed::{EmbeddedEditor, EmbeddedEditorBuilder};
use crate::render::CellSurface as Buffer;
use helix_core::{Rope, Transaction};
use helix_view::{document::Document, editor::Action, graphics::Rect, Editor};

use crate::compositor::{Component, RenderContext};
use crate::keymap::Keymaps;
use crate::ui::EditorView;

use super::model::StoryContext;

fn story_tokio_runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("storybook tokio runtime")
    })
}

/// A headless rendering stage that owns the `Editor` and `RuntimeIngress`
/// needed to render real components in a story.
///
/// Build the stage with [`Stage::new`], seed state with the builder methods
/// (`with_document`, `with_status`, ...), and then call [`Stage::render`] with
/// the component you want to draw.
pub(super) struct Stage {
    embedded: EmbeddedEditor,
    pub area: Rect,
}

impl Stage {
    /// Spin up a fresh editor + ingress sized to `area`, themed from `context`.
    pub fn new(area: Rect, context: StoryContext<'_>) -> Self {
        let runtime_handle = story_tokio_runtime().handle().clone();
        let _guard = runtime_handle.enter();
        let runtime = helix_runtime::Runtime::new(runtime_handle);
        let mut config = crate::config::Config::default();
        config.editor = context.editor_config.clone();
        let mut embedded = EmbeddedEditorBuilder::new(area, runtime)
            .config(config)
            .theme_loader(Arc::new(super::theme_loader()))
            .build()
            .expect("storybook embedded editor");
        embedded.editor_mut().set_theme(context.theme.clone());
        Self { embedded, area }
    }

    /// Open a document with the given `name` (relative path) and contents,
    /// making it the focused view. The document is created without touching
    /// the filesystem. It is considered "saved" — `is_modified` returns false.
    pub fn with_document(mut self, name: &str, text: &str) -> Self {
        let config = self.embedded.editor().config.clone();
        let syn_loader = self.embedded.editor().syn_loader.clone();
        let doc = Document::from(Rope::from(text), None, config, syn_loader);
        // `VerticalSplit` is the only action that works on an empty editor
        // (it doesn't require a focused view to exist).
        let doc_id = self
            .embedded
            .editor_mut()
            .new_file_from_document(Action::VerticalSplit, doc);
        if let Some(doc) = self.embedded.editor_mut().document_mut(doc_id) {
            doc.set_path(Some(&PathBuf::from(name)));
        }
        self
    }

    /// Open a document and apply a small edit on top so the bufferline and
    /// statusline render with the modified marker (`[+]`).
    pub fn with_modified_document(self, name: &str, text: &str) -> Self {
        let mut stage = self.with_document(name, text);
        let view_id = stage.embedded.editor().focused_view_id();
        let doc_id = stage
            .embedded
            .editor()
            .documents()
            .next()
            .map(|doc| doc.id())
            .expect("with_modified_document requires a document");
        if let Some(doc) = stage.embedded.editor_mut().document_mut(doc_id) {
            let len = doc.text().len_chars();
            let txn = Transaction::change(doc.text(), [(len, len, Some(" ".into()))].into_iter());
            let _ = doc.apply(&txn, view_id);
        }
        stage
    }

    /// Set the editor status message (rendered in the statusline).
    pub fn with_status(mut self, message: impl Into<String>) -> Self {
        self.embedded.editor_mut().set_status(message.into());
        self
    }

    pub fn editor(&self) -> &Editor {
        self.embedded.editor()
    }

    pub fn editor_mut(&mut self) -> &mut Editor {
        self.embedded.editor_mut()
    }

    pub fn ingress(&self) -> crate::runtime::RuntimeIngress {
        self.embedded.ingress()
    }

    /// Build a `RenderContext` that points at this stage's editor and ingress.
    /// Use this when you need to drive a component manually (e.g. to call
    /// `sync` before `render`, or to pass the context to a custom helper).
    pub fn render_context(&self) -> RenderContext<'_> {
        let editor = self.embedded.editor();
        let redraw = editor.redraw_handle();
        RenderContext::new(editor, self.embedded.ingress(), redraw, None)
    }

    /// Drive a component's `required_size` → `sync` → `render` lifecycle on
    /// this stage. Call repeatedly to layer multiple components in a single
    /// story (e.g. an EditorView backdrop with a Popup overlay).
    pub fn draw<C: Component>(&mut self, surface: &mut Buffer, component: &mut C) {
        self.draw_in(self.area, surface, component);
    }

    /// Like [`Stage::draw`] but renders into a sub-region of the stage area.
    pub fn draw_in<C: Component>(&mut self, area: Rect, surface: &mut Buffer, component: &mut C) {
        component.required_size((area.width, area.height));
        component.sync(self.embedded.editor_mut());
        let render_ctx = self.render_context();
        component.render(area, surface, &render_ctx);
    }

    /// Render a component that needs access to the prepared editor at
    /// construction time.
    pub fn render_with<C, F>(mut self, surface: &mut Buffer, build: F)
    where
        C: Component,
        F: FnOnce(&mut Editor) -> C,
    {
        let mut component = build(self.embedded.editor_mut());
        self.draw(surface, &mut component);
    }

    /// Render the real `EditorView` component as the only thing on the stage.
    pub fn render_editor_view(self, surface: &mut Buffer) {
        self.render_with(surface, |_editor| build_editor_view());
    }
}

/// Construct an `EditorView` with the default Helix engine and a fresh keymap.
/// Used by stories that want to render the real editor chrome.
pub(super) fn build_editor_view() -> EditorView {
    let factory = helix_modal::ModalEngineFactory::default();
    EditorView::from_modal_factory(
        Keymaps::default(),
        &factory,
        helix_view::editor::EditingEngineConfig::Helix,
    )
}
