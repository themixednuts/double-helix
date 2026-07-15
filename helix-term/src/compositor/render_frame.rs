use std::{
    borrow::Cow,
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};

use helix_core::Position;
use helix_runtime::{FrameHandle, FrameSource};
use helix_view::Editor;

use crate::runtime::RuntimeIngress;

/// Borrowed read-only editor view used while preparing a frame.
///
/// This deliberately exposes typed render queries instead of `&Editor`, so component render code
/// can only see the state that has been made part of the render contract. It is not an owned
/// render snapshot and must never cross the foreground boundary.
struct EditorRenderView<'a> {
    editor: &'a Editor,
}

impl<'a> EditorRenderView<'a> {
    fn new(editor: &'a Editor) -> Self {
        Self { editor }
    }

    pub fn config(&self) -> arc_swap::access::DynGuard<helix_view::editor::Config> {
        self.editor.config()
    }

    pub fn config_gen(&self) -> u64 {
        self.editor.config_gen
    }

    pub fn mode(&self) -> helix_view::document::Mode {
        self.editor.mode()
    }

    pub fn theme(&self) -> &'a helix_view::Theme {
        &self.editor.theme
    }

    pub fn theme_arc(&self) -> Arc<helix_view::Theme> {
        self.editor.theme_arc()
    }

    pub fn theme_name(&self) -> &'a str {
        self.editor.theme.name()
    }

    pub fn theme_generation(&self) -> u64 {
        self.editor.theme_generation
    }

    pub fn style(&self, scope: &str) -> helix_view::graphics::Style {
        self.editor.theme.get(scope)
    }

    pub fn assistant_theme(&self) -> &'a helix_view::Theme {
        self.editor.assistant_theme()
    }

    pub fn assistant_theme_arc(&self) -> Arc<helix_view::Theme> {
        self.editor.assistant_theme_arc()
    }

    pub fn popup_border(&self) -> bool {
        self.editor.popup_border()
    }

    pub fn menu_border(&self) -> bool {
        self.editor.menu_border()
    }

    pub fn autoinfo(&self) -> Option<&'a helix_view::info::Info> {
        self.editor.autoinfo.as_ref()
    }

    pub fn model_float_count(&self) -> usize {
        self.editor.model.floats.len()
    }

    pub fn cursor_position(&self) -> Option<Position> {
        self.editor.cursor().0
    }

    pub fn focused_view_id(&self) -> helix_view::ViewId {
        self.editor.focused_view_id()
    }

    pub fn focused_document_id(&self) -> helix_view::DocumentId {
        self.editor.focused_document_id()
    }

    pub fn document_count(&self) -> usize {
        self.editor.document_count()
    }

    pub fn documents(&self) -> impl Iterator<Item = &'a helix_view::Document> + 'a {
        self.editor.documents()
    }

    pub fn views(&self) -> impl Iterator<Item = (&'a helix_view::View, bool)> + 'a {
        self.editor.tree.views()
    }

    pub fn view(&self, id: helix_view::ViewId) -> Option<&'a helix_view::View> {
        self.editor.tree.try_get(id)
    }

    pub fn document(&self, id: helix_view::DocumentId) -> Option<&'a helix_view::Document> {
        self.editor.document(id)
    }

    pub fn component_document(
        &self,
        id: helix_view::DocumentId,
    ) -> Option<&'a helix_view::Document> {
        self.editor.component_docs.get(&id)
    }

    pub fn focused_document(&self) -> Option<&'a helix_view::Document> {
        self.document(self.focused_document_id())
    }

    pub fn component_document_count(&self) -> usize {
        self.editor.component_docs.len()
    }

    pub fn model_float_entries(
        &self,
    ) -> impl Iterator<Item = &'a helix_view::model::FloatEntry> + 'a {
        self.editor.model.floats.iter().map(|(_, entry)| entry)
    }

    pub fn panel_entry(
        &self,
        id: helix_view::model::PanelId,
    ) -> Option<&'a helix_view::model::PanelEntry> {
        self.editor.model.panels.get(id)
    }

    pub fn assistant_model(&self, focused: bool) -> helix_view::model::AssistantModel {
        self.editor.assistant_model(focused)
    }

    pub fn has_multiple_documents(&self) -> bool {
        self.editor.has_multiple_documents()
    }

    pub fn status_msg(&self) -> Option<(&'a str, helix_view::editor::Severity)> {
        self.editor
            .status_msg
            .as_ref()
            .map(|(message, severity)| (message.as_ref(), *severity))
    }

    pub fn macro_recording_register(&self) -> Option<char> {
        self.editor.macro_recording.as_ref().map(|(reg, _)| *reg)
    }

    pub fn cursor_cache(&self) -> &'a helix_view::editor::CursorCache {
        &self.editor.cursor_cache
    }

    pub fn syntax_loader(&self) -> &'a Arc<arc_swap::ArcSwap<helix_core::syntax::Loader>> {
        &self.editor.syn_loader
    }

    pub fn buffer_label(&self, doc: &helix_view::Document) -> String {
        self.editor.buffer_label(doc)
    }

    pub fn breakpoints_for_document(
        &self,
        doc: &'a helix_view::Document,
    ) -> Option<&'a [helix_view::editor::Breakpoint]> {
        doc.path()
            .and_then(|path| self.editor.breakpoints.get(path))
            .map(Vec::as_slice)
    }

    pub fn debug_execution_position(
        &self,
    ) -> Option<helix_view::gutter::DebugExecutionPosition<'a>> {
        let frame = self.editor.current_stack_frame()?;
        Some(helix_view::gutter::DebugExecutionPosition {
            line: frame.line.saturating_sub(1),
            path: frame
                .source
                .as_ref()
                .and_then(|source| source.path.as_deref()),
        })
    }

    pub fn workspace_diagnostic_counts(&self) -> helix_view::editor::WorkspaceDiagnosticCounts {
        self.editor.workspace_diagnostic_counts()
    }

    pub fn bench_overlay(&self) -> Option<helix_view::editor::BenchOverlay> {
        self.editor.bench_overlay()
    }

    pub fn first_register_value(&self, name: Option<char>) -> Option<Cow<'a, str>> {
        name.and_then(|name| self.editor.registers.first(name, self.editor))
    }

    pub fn notification_history(&self) -> &'a [helix_view::editor::Notification] {
        self.editor.get_notification_history()
    }

    pub fn work(&self) -> helix_runtime::Work {
        self.editor.work()
    }

    pub fn block(&self) -> helix_runtime::Block {
        self.editor.runtime().block().clone()
    }

    pub fn clock(&self) -> helix_runtime::Clock {
        self.editor.runtime().clock().clone()
    }
}

/// Immutable render context shared by all components during the render phase.
/// Created once per frame after sync + pre-render mutations complete.
pub struct RenderContext<'a> {
    editor: EditorRenderView<'a>,
    frame_area: helix_view::graphics::Rect,
    frame_time: Instant,
    /// Scroll offset communicated from parent (e.g. Popup) to child during render.
    /// Uses `AtomicUsize` for Sync-safe interior mutability. `usize::MAX` = None.
    scroll: AtomicUsize,
    pub ingress: RuntimeIngress,
    pub redraw: FrameHandle,
    frame_deadlines: Mutex<HashMap<FrameSource, Instant>>,
    render_steps: Mutex<Vec<crate::render::RenderStep>>,
}

const SCROLL_NONE: usize = usize::MAX;

impl<'a> RenderContext<'a> {
    pub fn new(editor: &'a Editor, ingress: RuntimeIngress, redraw: FrameHandle) -> Self {
        Self::with_scroll(editor, None, ingress, redraw, editor.tree.area())
    }

    pub fn with_scroll(
        editor: &'a Editor,
        scroll: Option<usize>,
        ingress: RuntimeIngress,
        redraw: FrameHandle,
        frame_area: helix_view::graphics::Rect,
    ) -> Self {
        RenderContext {
            editor: EditorRenderView::new(editor),
            frame_area,
            frame_time: Instant::now(),
            scroll: AtomicUsize::new(scroll.unwrap_or(SCROLL_NONE)),
            ingress,
            redraw,
            frame_deadlines: Mutex::new(HashMap::new()),
            render_steps: Mutex::new(Vec::new()),
        }
    }

    /// Request the next frame for an active animation.
    ///
    /// Requests are collected for the application scheduler after this render. A
    /// source that does not report a new deadline on the next frame is cancelled.
    pub fn request_frame_at(&self, source: FrameSource, deadline: Instant) {
        let mut deadlines = self
            .frame_deadlines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        deadlines
            .entry(source)
            .and_modify(|current| *current = (*current).min(deadline))
            .or_insert(deadline);
    }

    pub(crate) fn take_frame_deadlines(&self) -> HashMap<FrameSource, Instant> {
        let mut deadlines = self
            .frame_deadlines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *deadlines)
    }

    pub fn defer_paint(
        &self,
        name: &'static str,
        work: impl FnOnce(&mut crate::render::CellSurface, &crate::render::RenderCancellation)
            + Send
            + 'static,
    ) {
        self.render_steps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(crate::render::RenderStep::paint(name, work));
    }

    pub(crate) fn defer_stateful_paint(
        &self,
        name: &'static str,
        work: impl FnOnce(
                &mut crate::render::CellSurface,
                &mut crate::render::CacheStore,
                &mut crate::render::RenderMetadata,
                &crate::render::RenderCancellation,
            ) + Send
            + 'static,
    ) {
        self.render_steps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(crate::render::RenderStep::stateful(name, work));
    }

    pub fn defer_prepared(&self, name: &'static str, batch: Vec<crate::render::PreparedRender>) {
        let Some(step) = crate::render::RenderStep::prepared(name, batch) else {
            return;
        };
        self.render_steps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(step);
    }

    pub(crate) fn take_render_steps(&self) -> Vec<crate::render::RenderStep> {
        let mut steps = self
            .render_steps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *steps)
    }

    pub fn config(&self) -> arc_swap::access::DynGuard<helix_view::editor::Config> {
        self.editor.config()
    }

    pub fn frame_area(&self) -> helix_view::graphics::Rect {
        self.frame_area
    }

    pub fn focus_target(&self) -> helix_view::model::FocusTarget {
        self.editor.editor.model.focus
    }

    pub fn frame_time(&self) -> Instant {
        self.frame_time
    }

    pub fn config_gen(&self) -> u64 {
        self.editor.config_gen()
    }

    pub fn mode(&self) -> helix_view::document::Mode {
        self.editor.mode()
    }

    pub fn theme(&self) -> &'a helix_view::Theme {
        self.editor.theme()
    }

    pub fn theme_arc(&self) -> Arc<helix_view::Theme> {
        self.editor.theme_arc()
    }

    pub fn theme_name(&self) -> &'a str {
        self.editor.theme_name()
    }

    pub fn theme_generation(&self) -> u64 {
        self.editor.theme_generation()
    }

    pub fn style(&self, scope: &str) -> helix_view::graphics::Style {
        self.editor.style(scope)
    }

    pub fn assistant_theme(&self) -> &'a helix_view::Theme {
        self.editor.assistant_theme()
    }

    pub fn assistant_theme_arc(&self) -> Arc<helix_view::Theme> {
        self.editor.assistant_theme_arc()
    }

    pub fn popup_border(&self) -> bool {
        self.editor.popup_border()
    }

    pub fn menu_border(&self) -> bool {
        self.editor.menu_border()
    }

    pub fn autoinfo(&self) -> Option<&'a helix_view::info::Info> {
        self.editor.autoinfo()
    }

    pub fn model_float_count(&self) -> usize {
        self.editor.model_float_count()
    }

    pub fn cursor_position(&self) -> Option<Position> {
        self.editor.cursor_position()
    }

    pub fn focused_view_id(&self) -> helix_view::ViewId {
        self.editor.focused_view_id()
    }

    pub fn focused_document_id(&self) -> helix_view::DocumentId {
        self.editor.focused_document_id()
    }

    pub fn document_count(&self) -> usize {
        self.editor.document_count()
    }

    pub fn documents(&self) -> impl Iterator<Item = &'a helix_view::Document> + 'a {
        self.editor.documents()
    }

    pub fn views(&self) -> impl Iterator<Item = (&'a helix_view::View, bool)> + 'a {
        self.editor.views()
    }

    pub fn view(&self, id: helix_view::ViewId) -> Option<&'a helix_view::View> {
        self.editor.view(id)
    }

    pub fn document(&self, id: helix_view::DocumentId) -> Option<&'a helix_view::Document> {
        self.editor.document(id)
    }

    pub fn component_document(
        &self,
        id: helix_view::DocumentId,
    ) -> Option<&'a helix_view::Document> {
        self.editor.component_document(id)
    }

    pub fn focused_document(&self) -> Option<&'a helix_view::Document> {
        self.editor.focused_document()
    }

    pub fn component_document_count(&self) -> usize {
        self.editor.component_document_count()
    }

    pub fn model_float_entries(
        &self,
    ) -> impl Iterator<Item = &'a helix_view::model::FloatEntry> + 'a {
        self.editor.model_float_entries()
    }

    pub fn panel_entry(
        &self,
        id: helix_view::model::PanelId,
    ) -> Option<&'a helix_view::model::PanelEntry> {
        self.editor.panel_entry(id)
    }

    pub fn assistant_model(&self, focused: bool) -> helix_view::model::AssistantModel {
        self.editor.assistant_model(focused)
    }

    pub fn has_multiple_documents(&self) -> bool {
        self.editor.has_multiple_documents()
    }

    pub fn status_msg(&self) -> Option<(&'a str, helix_view::editor::Severity)> {
        self.editor.status_msg()
    }

    pub fn macro_recording_register(&self) -> Option<char> {
        self.editor.macro_recording_register()
    }

    pub fn cursor_cache(&self) -> &'a helix_view::editor::CursorCache {
        self.editor.cursor_cache()
    }

    pub fn syntax_loader(&self) -> &'a Arc<arc_swap::ArcSwap<helix_core::syntax::Loader>> {
        self.editor.syntax_loader()
    }

    pub fn buffer_label(&self, doc: &helix_view::Document) -> String {
        self.editor.buffer_label(doc)
    }

    pub fn breakpoints_for_document(
        &self,
        doc: &'a helix_view::Document,
    ) -> Option<&'a [helix_view::editor::Breakpoint]> {
        self.editor.breakpoints_for_document(doc)
    }

    pub fn debug_execution_position(
        &self,
    ) -> Option<helix_view::gutter::DebugExecutionPosition<'a>> {
        self.editor.debug_execution_position()
    }

    pub fn workspace_diagnostic_counts(&self) -> helix_view::editor::WorkspaceDiagnosticCounts {
        self.editor.workspace_diagnostic_counts()
    }

    pub fn bench_overlay(&self) -> Option<helix_view::editor::BenchOverlay> {
        self.editor.bench_overlay()
    }

    pub fn first_register_value(&self, name: Option<char>) -> Option<Cow<'a, str>> {
        self.editor.first_register_value(name)
    }

    pub fn notification_history(&self) -> &'a [helix_view::editor::Notification] {
        self.editor.notification_history()
    }

    pub fn work(&self) -> helix_runtime::Work {
        self.editor.work()
    }

    pub fn block(&self) -> helix_runtime::Block {
        self.editor.block()
    }

    pub fn clock(&self) -> helix_runtime::Clock {
        self.editor.clock()
    }

    pub fn scroll(&self) -> Option<usize> {
        let value = self.scroll.load(Ordering::Relaxed);
        if value == SCROLL_NONE {
            None
        } else {
            Some(value)
        }
    }

    pub fn set_scroll(&self, value: Option<usize>) {
        self.scroll
            .store(value.unwrap_or(SCROLL_NONE), Ordering::Relaxed);
    }
}
