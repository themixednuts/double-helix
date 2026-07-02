use crate::{
    commands::{self, OnKeyCallback, OnKeyCallbackKind},
    compositor::{Component, Event, EventResult, RenderContext},
    handlers::completion::CompletionItem,
    key,
    keymap::Keymaps,
    render::{blit_cells, CacheStore, CellSurface, PreparedRender},
    ui::{
        document::{render_document, HighlighterInput, LinePos, RenderOutput, TextRenderer},
        statusline,
        text_decorations::{
            self, Decoration, DecorationManager, FoldDecoration, InlineDiagnostics,
            PluginDecoration,
        },
        Completion, NotificationPopup, ProgressSpinners,
    },
    widgets::{tabs_with_options, Tab, TabCell, TabsOptions, TabsScrollPolicy, TabsStyle},
};

use helix_core::{
    diagnostic::NumberOrString, movement::Direction, text_annotations::TextAnnotations,
    visual_offset_from_block, Position, Range, Selection,
};
use helix_loader::VERSION_AND_GIT_HASH;
use helix_view::{
    // annotations::diagnostics::DiagnosticFilter,
    document::Mode,
    editor::{CompleteAction, Config, CursorCache, InlineBlameConfig, InlineBlameShow},
    graphics::{Color, CursorKind, Modifier, Rect, Style},
    gutter::{DebugExecutionPosition, GutterContext},
    icons::ICONS,
    input::{KeyEvent, MouseButton, MouseEvent, MouseEventKind},
    keyboard::{KeyCode, KeyModifiers},
    Document,
    DocumentId,
    Editor,
    Theme,
    View,
    ViewId,
};
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet, VecDeque},
    mem::take,
    rc::Rc,
    sync::{Arc, LazyLock},
};

use tui::text::{Span, Spans};

use super::text_decorations::blame::InlineBlame;

use helix_view::engine::{KeymapQuery, ModalInputState};
use helix_view::model::FocusTarget;
use helix_view::view::{
    LayoutSnapshot, LineMap, RefreshState, RenderScope, RenderSnapshots, RenderSnapshotsRef,
    RenderState, ReuseState, SyntaxStyleCache,
};

const MAX_SEED_LINE_MAP_GAP: usize = 4_096;

/// View render context grouping parameters for `render_view`.
pub(crate) struct ViewRenderContext<'a> {
    pub doc: &'a Document,
    pub view: &'a View,
    pub viewport: Rect,
    pub is_focused: bool,
    pub config: &'a Config,
    pub config_gen: u64,
    pub theme: &'a Theme,
    pub mode: Mode,
    pub syntax_loader: &'a Arc<arc_swap::ArcSwap<helix_core::syntax::Loader>>,
    pub cursor_cache: &'a CursorCache,
    pub gutter_context: GutterContext<'a>,
    pub debug_execution: Option<DebugExecutionPosition<'a>>,
    /// Cached syntax styles to replay instead of running tree-sitter.
    pub cached_syntax: Option<&'a SyntaxStyleCache>,
    /// Set of dirty visual rows. Only these rows will be re-rendered.
    /// `None` means all rows are dirty (full render).
    pub dirty_rows: Option<&'a std::collections::HashSet<u16>>,
    pub seed_line_map: Option<&'a LineMap>,
}

struct ViewRenderCacheEntry {
    snapshots: RenderSnapshots,
    /// The rendered cells for the view's area.
    cells: CellSurface,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct ViewRenderCacheKey {
    view: ViewId,
    doc: DocumentId,
}

fn copy_cell_region(src: &CellSurface, area: Rect) -> CellSurface {
    let mut buf = CellSurface::empty(tui::ratatui::to_ratatui_rect(area));
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let (Some(src_cell), Some(dst_cell)) = (src.cell((x, y)), buf.cell_mut((x, y))) {
                *dst_cell = src_cell.clone();
            }
        }
    }
    buf
}

impl ViewRenderCacheKey {
    const fn new(view: ViewId, doc: DocumentId) -> Self {
        Self { view, doc }
    }
}

struct ViewFrame<'a> {
    snapshot: ViewFrameSnapshot<'a>,
    doc: &'a Document,
    trace: ViewTrace<'a>,
    area: Rect,
}

struct ViewFrameSnapshot<'a> {
    config: arc_swap::access::DynGuard<Config>,
    config_gen: u64,
    theme: &'a Theme,
    theme_name: &'a str,
    mode: Mode,
    syntax_loader: &'a Arc<arc_swap::ArcSwap<helix_core::syntax::Loader>>,
    cursor_cache: &'a CursorCache,
    focused_view_id: ViewId,
    breakpoints: Option<&'a [helix_view::editor::Breakpoint]>,
    debug_execution: Option<DebugExecutionPosition<'a>>,
}

struct ViewTrace<'a> {
    view: &'a View,
    selection: &'a Selection,
    is_focused: bool,
    frame_num: u64,
    view_render_start: std::time::Instant,
    render_start: std::time::Instant,
}

struct RenderRequest<'a> {
    cached_syntax: Option<&'a SyntaxStyleCache>,
    dirty_rows: Option<&'a HashSet<u16>>,
    seed_line_map: Option<&'a LineMap>,
}

#[derive(Clone, Copy)]
enum RenderPhase {
    Dirty { rows: usize },
    Full,
}

impl RenderPhase {
    fn total_phase(self) -> &'static str {
        match self {
            Self::Dirty { .. } => "dirty_total",
            Self::Full => "full_total",
        }
    }

    fn path_label(self) -> &'static str {
        match self {
            Self::Dirty { .. } => "dirty",
            Self::Full => "full",
        }
    }
}

impl ViewFrame<'_> {
    fn render_state<'a>(
        &self,
        cached: Option<RenderSnapshotsRef<'a>>,
        terminal_focused: bool,
    ) -> RenderState {
        self.view().resolve_render_state(
            self.doc,
            self.snapshot.config_gen,
            Arc::from(self.snapshot.theme_name),
            cached,
            self.render_scope(terminal_focused),
        )
    }

    fn render_scope(&self, terminal_focused: bool) -> RenderScope<'_> {
        RenderScope::new(
            self.trace.selection,
            self.snapshot.mode,
            self.trace.is_focused,
            terminal_focused,
        )
    }

    fn view(&self) -> &View {
        self.trace.view
    }

    fn view_id(&self) -> ViewId {
        self.view().id
    }

    fn cache_key(&self) -> ViewRenderCacheKey {
        ViewRenderCacheKey::new(self.view_id(), self.doc.id())
    }

    fn update_cursor_cache(&self, layout: &LayoutSnapshot) {
        if self.snapshot.focused_view_id == self.view_id() {
            self.snapshot
                .cursor_cache
                .set(layout.cursor_position(self.doc, self.view()));
        }
    }

    fn clear_dirty_rows(&self, dirty_rows: &HashSet<u16>, surface: &mut CellSurface) {
        let inner = self.view().inner_area(self.doc);
        for &row in dirty_rows {
            if row < inner.height {
                let y = inner.y + row;
                tui::ratatui::widgets::Widget::render(
                    tui::ratatui::widgets::Clear,
                    tui::ratatui::to_ratatui_rect(Rect::new(inner.x, y, inner.width, 1)),
                    surface,
                );
            }
        }
    }

    fn render_context<'a>(
        &'a self,
        cached_syntax: Option<&'a SyntaxStyleCache>,
        dirty_rows: Option<&'a HashSet<u16>>,
        seed_line_map: Option<&'a LineMap>,
    ) -> ViewRenderContext<'a> {
        let wrap_indicator = self
            .snapshot
            .config
            .soft_wrap
            .wrap_indicator
            .as_deref()
            .map_or(Cow::Borrowed("↪"), Cow::Borrowed);
        ViewRenderContext {
            doc: self.doc,
            view: self.view(),
            viewport: self.area,
            is_focused: self.trace.is_focused,
            config: &self.snapshot.config,
            config_gen: self.snapshot.config_gen,
            theme: self.snapshot.theme,
            mode: self.snapshot.mode,
            syntax_loader: self.snapshot.syntax_loader,
            cursor_cache: self.snapshot.cursor_cache,
            gutter_context: GutterContext {
                mode: self.snapshot.mode,
                line_number: self.snapshot.config.line_number,
                wrap_indicator,
                breakpoints: self.snapshot.breakpoints,
                debug_execution: self.snapshot.debug_execution,
            },
            debug_execution: self.snapshot.debug_execution,
            cached_syntax,
            dirty_rows,
            seed_line_map,
        }
    }
}

impl ViewTrace<'_> {
    fn log_state(&self) {
        log::debug!(
            "[view] id={:?} focused={} area=({},{} {}x{}) inner_height={} statusline_row={}",
            self.view.id,
            self.is_focused,
            self.view.area.x,
            self.view.area.y,
            self.view.area.width,
            self.view.area.height,
            self.view.area.height.saturating_sub(1),
            self.view.area.y + self.view.area.height.saturating_sub(1),
        );
    }

    fn log_reuse(&self, reuse: &ReuseState) {
        let sel = self.selection.primary();
        let (sel_start, sel_end) = if sel.anchor <= sel.head {
            (sel.anchor, sel.head)
        } else {
            (sel.head, sel.anchor)
        };
        let intersecting_rows: Vec<u16> = reuse
            .line_map()
            .lines
            .iter()
            .filter(|line| sel_start < line.char_range_end && sel_end >= line.char_range_start)
            .map(|line| line.visual_row)
            .collect();
        log::info!(
            "F{} CACHE HIT sel=({},{}) dirty={:?} sel_rows={:?} lines={}",
            self.frame_num,
            sel.anchor,
            sel.head,
            reuse.dirty_rows(),
            intersecting_rows,
            reuse.line_count(),
        );
    }

    fn log_pure_reuse(&self) {
        log::info!(
            "F{} CACHE PURE_BLIT view={:?}",
            self.frame_num,
            self.view.id
        );
    }

    fn log_dirty_reuse(&self, reuse: &ReuseState, output: &RenderOutput) {
        log::info!(
            concat!(
                "F{} CACHE DIRTY_RERENDER view={:?} dirty_rows={:?}",
                " new_syntax={} new_linemap={}",
                " syntax_advances={} skip_right_syntax_advances={} skip_right_eof_fast_paths={}",
                " elapsed={:?}"
            ),
            self.frame_num,
            self.view.id,
            reuse.dirty_rows(),
            output.syntax_styles.len(),
            output.line_map.lines.len(),
            output.metrics.syntax_advances,
            output.metrics.skip_right_syntax_advances,
            output.metrics.skip_right_eof_fast_paths,
            self.render_start.elapsed(),
        );
    }

    fn log_refresh(&self, refresh: &RefreshState, output: &RenderOutput) {
        let view_position = refresh.view_position();
        log::info!(
            concat!(
                "F{} CACHE MISS view={:?} anchor={} voff={} area={}x{} syntax={} lines={}",
                " syntax_advances={} skip_right_syntax_advances={} skip_right_eof_fast_paths={}",
                " elapsed={:?}"
            ),
            self.frame_num,
            self.view.id,
            view_position.anchor,
            view_position.vertical_offset,
            self.view.area.width,
            self.view.area.height,
            output.syntax_styles.len(),
            output.line_map.lines.len(),
            output.metrics.syntax_advances,
            output.metrics.skip_right_syntax_advances,
            output.metrics.skip_right_eof_fast_paths,
            self.render_start.elapsed(),
        );
    }

    fn log_area_phase(&self, phase: &'static str, start: std::time::Instant) {
        helix_view::bench::log_run_phase("editor_render_view", phase, start.elapsed(), || {
            format!(
                "view_id={:?} area={}x{}",
                self.view.id, self.view.area.width, self.view.area.height
            )
        });
    }

    fn log_overlay_fingerprints(&self, start: std::time::Instant, rows: usize) {
        helix_view::bench::log_run_phase(
            "editor_render_view",
            "overlay_fingerprints",
            start.elapsed(),
            || format!("view_id={:?} rows={}", self.view.id, rows),
        );
    }

    fn log_blit(&self, start: std::time::Instant) {
        self.log_area_phase("blit", start);
    }

    fn log_copy_region(&self, start: std::time::Instant) {
        self.log_area_phase("copy_region", start);
    }

    fn log_render_phase(&self, phase: RenderPhase) {
        match phase {
            RenderPhase::Dirty { rows } => {
                helix_view::bench::log_run_phase(
                    "editor_render_view",
                    "dirty_render_view",
                    self.render_start.elapsed(),
                    || format!("view_id={:?} dirty_rows={}", self.view.id, rows),
                );
            }
            RenderPhase::Full => helix_view::bench::log_run_phase(
                "editor_render_view",
                "full_render_view",
                self.view_render_start.elapsed(),
                || format!("view_id={:?} path=full_before_fp", self.view.id),
            ),
        }
    }

    fn log_render_total(&self, phase: RenderPhase) {
        helix_view::bench::log_run_phase(
            "editor_render_view",
            phase.total_phase(),
            self.view_render_start.elapsed(),
            || format!("view_id={:?} path={}", self.view.id, phase.path_label()),
        );
    }
}

/// Per-view render cache with two-tier invalidation:
///
/// 1. **Content hit** — content state matches → blit cached cells, compute dirty
///    lines from overlay fingerprints, re-render only dirty lines. When 0 lines
///    are dirty (nothing changed at all), this is a pure blit — zero extra work.
/// 2. **Content miss** — content changed → full re-render + record syntax styles + line map
const VIEW_RENDER_CACHE_DOCS_PER_VIEW: usize = 8;

#[derive(Default)]
struct ViewRenderCache {
    entries: HashMap<ViewRenderCacheKey, ViewRenderCacheEntry>,
    order: VecDeque<ViewRenderCacheKey>,
    #[cfg(debug_assertions)]
    hits: u64,
    #[cfg(debug_assertions)]
    misses: u64,
    #[cfg(debug_assertions)]
    dirty_lines: u64,
    #[cfg(debug_assertions)]
    clean_lines: u64,
    #[cfg(debug_assertions)]
    frames: u64,
}

impl ViewRenderCache {
    fn update_overlay_fingerprints(
        &mut self,
        key: ViewRenderCacheKey,
        overlay_fingerprints: Arc<[u64]>,
    ) {
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.snapshots.paint.overlay_fingerprints = overlay_fingerprints;
        }
    }

    fn store(&mut self, key: ViewRenderCacheKey, snapshots: RenderSnapshots, cells: CellSurface) {
        self.order.retain(|cached| *cached != key);
        self.order.push_back(key);
        self.entries
            .insert(key, ViewRenderCacheEntry { snapshots, cells });
        self.evict_view(key.view);
    }

    fn retain_active_views(&mut self, active: &HashSet<ViewId>) {
        self.entries.retain(|key, _| active.contains(&key.view));
        self.order.retain(|key| active.contains(&key.view));
    }

    fn evict_view(&mut self, view: ViewId) {
        while self.order.iter().filter(|key| key.view == view).count()
            > VIEW_RENDER_CACHE_DOCS_PER_VIEW
        {
            let Some(position) = self.order.iter().position(|key| key.view == view) else {
                return;
            };
            if let Some(key) = self.order.remove(position) {
                self.entries.remove(&key);
            }
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }

    #[cfg(debug_assertions)]
    fn record_hit(&mut self, dirty_rows: usize, total_lines: usize) {
        self.hits = self.hits.wrapping_add(1);
        self.dirty_lines = self.dirty_lines.wrapping_add(dirty_rows as u64);
        self.clean_lines = self
            .clean_lines
            .wrapping_add(total_lines.saturating_sub(dirty_rows) as u64);
    }

    #[cfg(not(debug_assertions))]
    fn record_hit(&mut self, _dirty_rows: usize, _total_lines: usize) {}

    #[cfg(debug_assertions)]
    fn record_miss(&mut self) {
        self.misses = self.misses.wrapping_add(1);
    }

    #[cfg(not(debug_assertions))]
    fn record_miss(&mut self) {}

    #[cfg(debug_assertions)]
    fn record_frame(&mut self) {
        self.frames = self.frames.wrapping_add(1);
    }

    #[cfg(not(debug_assertions))]
    fn record_frame(&mut self) {}

    #[cfg(debug_assertions)]
    fn log_and_reset_stats(&mut self) {
        if self.frames.is_multiple_of(300) {
            log::debug!(
                "ViewRenderCache: {} hits ({} dirty / {} clean lines), {} misses over 300 frames",
                self.hits,
                self.dirty_lines,
                self.clean_lines,
                self.misses,
            );
            self.hits = 0;
            self.dirty_lines = 0;
            self.clean_lines = 0;
            self.misses = 0;
        }
    }

    #[cfg(not(debug_assertions))]
    fn log_and_reset_stats(&mut self) {}
}

struct BufferlineModel<'a> {
    theme: &'a Theme,
    separator: String,
    current_doc: DocumentId,
    documents: Vec<BufferlineDocument<'a>>,
}

struct BufferlineDocument<'a> {
    id: DocumentId,
    label: String,
    path: Option<&'a std::path::PathBuf>,
    language_name: Option<&'a str>,
    is_modified: bool,
}

impl<'a> BufferlineModel<'a> {
    fn from_render_context(cx: &RenderContext<'a>, separator: &str) -> Self {
        Self {
            theme: cx.theme(),
            separator: separator.to_owned(),
            current_doc: cx.focused_document_id(),
            documents: cx
                .documents()
                .map(|doc| BufferlineDocument {
                    id: doc.id(),
                    label: cx.buffer_label(doc),
                    path: doc.path(),
                    language_name: doc.language_name(),
                    is_modified: doc.is_modified(),
                })
                .collect(),
        }
    }
}

pub struct EditorView {
    pub keymaps: Keymaps,
    /// The editing engine. Always `Some` during normal operation; temporarily
    /// `None` during `feed_key` to satisfy the borrow checker (engine and
    /// editor are disjoint, but the compiler can't prove it).
    engine: Option<Box<dyn helix_view::engine::EditingEngine>>,
    /// Shared command registry — borrowed by Context for MappableCommand::Engine execution.
    pub(crate) registry: std::sync::Arc<helix_modal::registry::CommandRegistry>,
    on_next_key: Option<(OnKeyCallback, OnKeyCallbackKind)>,
    pseudo_pending: Vec<KeyEvent>,
    pub(crate) completion: Option<Completion>,
    spinners: ProgressSpinners,
    bufferline_info: BufferLineInfo,
    /// Tracks if the terminal window is focused by reaction to terminal focus events
    terminal_focused: bool,
    /// Tracks if there are prompt layers active (updated by compositor)
    pub prompt_active: bool,
    notification_popup: NotificationPopup,
    /// Per-view render cache for skipping re-render of unchanged views.
    view_cache: ViewRenderCache,
    chrome_cache: CacheStore,
}

impl EditorView {
    fn content_area(view: &View) -> Rect {
        view.area.clip_bottom(1)
    }

    fn engine_input_state(&self) -> ModalInputState {
        self.engine
            .as_ref()
            .map_or_else(ModalInputState::default, |engine| engine.input_state())
    }

    fn set_engine_input_state(&mut self, state: ModalInputState) {
        if let Some(engine) = self.engine.as_mut() {
            engine.set_input_state(state);
        }
    }

    fn sync_context_from_engine(&self, cx: &mut commands::Context) {
        let state = self.engine_input_state();
        cx.count = state.count;
        cx.register = state.selected_register;
    }

    fn sync_engine_from_context(&mut self, cx: &mut commands::Context) {
        let state = ModalInputState {
            count: cx.count,
            selected_register: cx.register,
        };
        self.set_engine_input_state(state);
        cx.editor.frontend_mut().focused_modal_input = state;
    }

    fn publish_focused_modal_input(&self, editor: &mut Editor) {
        editor.frontend_mut().focused_modal_input = self.engine_input_state();
    }

    fn prepare_statusline(
        &self,
        cx: &RenderContext,
        doc: &Document,
        view: &View,
        is_focused: bool,
    ) -> PreparedRender {
        let statusline_area = view.area.clip_top(view.area.height.saturating_sub(1));
        let config = cx.config();
        let statusline_model = statusline::StatuslineModel::collect(
            statusline::StatuslineContext {
                config: &config.statusline,
                theme: cx.theme(),
                theme_name: cx.theme_name(),
                color_modes: config.color_modes,
                workspace_diagnostics: cx.workspace_diagnostic_counts(),
                bench_overlay: cx.bench_overlay(),
                mode: cx.mode(),
                selected_register: self.engine_input_state().selected_register,
                spinners: &self.spinners,
            },
            doc,
            view,
            is_focused,
        );
        statusline::Statusline::prepare(statusline_model, statusline_area)
    }

    fn new(
        keymaps: Keymaps,
        engine: Box<dyn helix_view::engine::EditingEngine>,
        registry: std::sync::Arc<helix_modal::registry::CommandRegistry>,
    ) -> Self {
        Self {
            keymaps,
            engine: Some(engine),
            registry,
            on_next_key: None,
            pseudo_pending: Vec::new(),
            completion: None,
            spinners: ProgressSpinners::default(),
            bufferline_info: BufferLineInfo::default(),
            terminal_focused: true,
            prompt_active: false,
            notification_popup: NotificationPopup::new(),
            view_cache: ViewRenderCache::default(),
            chrome_cache: CacheStore::default(),
        }
    }

    pub fn from_modal_factory(
        keymaps: Keymaps,
        factory: &helix_modal::ModalEngineFactory,
        engine_config: helix_view::editor::EditingEngineConfig,
    ) -> Self {
        Self::new(
            keymaps,
            factory.create_engine(engine_config),
            factory.registry(),
        )
    }

    fn blit_cached_view(
        cache: &ViewRenderCache,
        key: ViewRenderCacheKey,
        surface: &mut CellSurface,
    ) {
        if let Some(cached) = cache.entries.get(&key) {
            blit_cells(&cached.cells, surface);
        }
    }

    fn store_view_render(
        cache: &mut ViewRenderCache,
        frame: &ViewFrame<'_>,
        snapshots: RenderSnapshots,
        surface: &CellSurface,
    ) {
        frame.update_cursor_cache(&snapshots.layout);
        let copy_start = std::time::Instant::now();
        let cells = copy_cell_region(surface, Self::content_area(frame.view()));
        frame.trace.log_copy_region(copy_start);
        cache.store(frame.cache_key(), snapshots, cells);
    }

    fn render_pass(
        &self,
        frame: &ViewFrame<'_>,
        surface: &mut CellSurface,
        request: RenderRequest<'_>,
        phase: RenderPhase,
    ) -> RenderOutput {
        let vctx = frame.render_context(
            request.cached_syntax,
            request.dirty_rows,
            request.seed_line_map,
        );
        let output = self.render_view(&vctx, surface);
        frame.trace.log_render_phase(phase);
        output
    }

    fn render_reuse_plan(
        &self,
        cache: &mut ViewRenderCache,
        frame: ViewFrame<'_>,
        surface: &mut CellSurface,
        reuse: ReuseState,
    ) {
        let blit_start = std::time::Instant::now();
        Self::blit_cached_view(cache, frame.cache_key(), surface);
        frame.trace.log_blit(blit_start);

        frame.update_cursor_cache(&reuse.layout_snapshot());
        frame.trace.log_reuse(&reuse);

        cache.record_hit(reuse.dirty_count(), reuse.line_count());

        if reuse.is_clean() {
            frame.trace.log_pure_reuse();
            cache.update_overlay_fingerprints(frame.cache_key(), reuse.overlay_fingerprints());
            return;
        }

        frame.clear_dirty_rows(reuse.dirty_rows(), surface);
        let phase = RenderPhase::Dirty {
            rows: reuse.dirty_count(),
        };

        let render_output = self.render_pass(
            &frame,
            surface,
            RenderRequest {
                cached_syntax: Some(reuse.syntax_styles()),
                dirty_rows: Some(reuse.dirty_rows()),
                seed_line_map: Some(reuse.line_map()),
            },
            phase,
        );

        frame.trace.log_dirty_reuse(&reuse, &render_output);

        let snapshots = reuse.into_snapshots(render_output.line_map, render_output.syntax_styles);
        Self::store_view_render(cache, &frame, snapshots, surface);
        frame.trace.log_render_total(phase);
    }

    fn render_refresh_plan(
        &self,
        cache: &mut ViewRenderCache,
        frame: ViewFrame<'_>,
        surface: &mut CellSurface,
        refresh: RefreshState,
    ) {
        cache.record_miss();

        let render_output = self.render_pass(
            &frame,
            surface,
            RenderRequest {
                cached_syntax: None,
                dirty_rows: None,
                seed_line_map: refresh.seed_line_map(),
            },
            RenderPhase::Full,
        );

        let fp_start = std::time::Instant::now();
        let overlay_fingerprints = refresh.overlay_fingerprints(
            &render_output.line_map,
            frame.render_scope(self.terminal_focused),
        );
        frame
            .trace
            .log_overlay_fingerprints(fp_start, render_output.line_map.lines.len());

        frame.trace.log_refresh(&refresh, &render_output);

        let snapshots = refresh.into_snapshots(
            render_output.line_map,
            render_output.syntax_styles,
            overlay_fingerprints,
        );
        Self::store_view_render(cache, &frame, snapshots, surface);
        frame.trace.log_render_total(RenderPhase::Full);
    }

    fn render_views(
        &self,
        area: Rect,
        surface: &mut CellSurface,
        cx: &RenderContext,
        cache: &mut ViewRenderCache,
        frame_num: u64,
        render_start: std::time::Instant,
    ) {
        cache.record_frame();

        log::debug!(
            "[editor_render] area=({},{} {}x{}) views={}",
            area.x,
            area.y,
            area.width,
            area.height,
            cx.views().count(),
        );
        for (view, is_focused) in cx.views() {
            let view_render_start = std::time::Instant::now();
            let doc = cx.document(view.doc).unwrap();
            let selection = doc.selection(view.id);

            let frame = ViewFrame {
                snapshot: ViewFrameSnapshot {
                    config: cx.config(),
                    config_gen: cx.config_gen(),
                    theme: cx.theme(),
                    theme_name: cx.theme_name(),
                    mode: cx.mode(),
                    syntax_loader: cx.syntax_loader(),
                    cursor_cache: cx.cursor_cache(),
                    focused_view_id: cx.focused_view_id(),
                    breakpoints: cx.breakpoints_for_document(doc),
                    debug_execution: cx.debug_execution_position(),
                },
                doc,
                trace: ViewTrace {
                    view,
                    selection,
                    is_focused,
                    frame_num,
                    view_render_start,
                    render_start,
                },
                area,
            };
            frame.trace.log_state();

            match frame.render_state(
                cache
                    .entries
                    .get(&ViewRenderCacheKey::new(view.id, view.doc))
                    .map(|entry| entry.snapshots.as_ref()),
                self.terminal_focused,
            ) {
                RenderState::Reuse(reuse) => self.render_reuse_plan(cache, frame, surface, reuse),
                RenderState::Refresh(refresh) => {
                    self.render_refresh_plan(cache, frame, surface, refresh)
                }
            }
        }
    }

    pub fn spinners_mut(&mut self) -> &mut ProgressSpinners {
        &mut self.spinners
    }

    pub fn draw_welcome(theme: &Theme, view: &View, surface: &mut CellSurface, is_colorful: bool) {
        /// Logo for Helix
        const LOGO_STR: &str = "\
**             
*****        ::
 ******** :::::
     **::::::: 
   ::::::::***=
:::::::    ====
::::    =======
:---========   
 =======--     
===== -------- 
==        -----
             --";

        /// Size of the maximum line of the logo
        static LOGO_WIDTH: LazyLock<u16> = LazyLock::new(|| {
            LOGO_STR
                .lines()
                .max_by(|line, other| line.len().cmp(&other.len()))
                .unwrap_or("")
                .len() as u16
        });

        /// Use when true color is not supported
        static LOGO_NO_COLOR: LazyLock<Vec<Spans>> = LazyLock::new(|| {
            LOGO_STR
                .lines()
                .map(|line| Spans(vec![Span::raw(line)]))
                .collect()
        });

        /// The logo is colored using Helix's colors
        static LOGO_WITH_COLOR: LazyLock<Vec<Spans>> = LazyLock::new(|| {
            LOGO_STR
                .lines()
                .map(|line| {
                    line.chars()
                        .map(|ch| match ch {
                            '*' | ':' | '=' | '-' => Span::styled(
                                ch.to_string(),
                                Style::new().fg(match ch {
                                    // Dark purple
                                    '*' => Color::Rgb(112, 107, 200),
                                    // Dark blue
                                    ':' => Color::Rgb(132, 221, 234),
                                    // Bright purple
                                    '=' => Color::Rgb(153, 123, 200),
                                    // Bright blue
                                    '-' => Color::Rgb(85, 197, 228),
                                    _ => unreachable!(),
                                }),
                            ),
                            ' ' => Span::raw(" "),
                            _ => unreachable!("logo should only contain '*', ':', '=', '-' or ' '"),
                        })
                        .collect()
                })
                .collect()
        });

        /// How much space to put between the help text and the logo
        const LOGO_LEFT_PADDING: u16 = 6;

        // Shift the help text to the right by this amount, to add space
        // for the logo
        static HELP_X_LOGO_OFFSET: LazyLock<u16> =
            LazyLock::new(|| *LOGO_WIDTH / 2 + LOGO_LEFT_PADDING / 2);

        #[derive(PartialEq, PartialOrd, Eq, Ord)]
        enum AlignLine {
            Left,
            Center,
        }
        use AlignLine::*;

        let logo = if is_colorful {
            &LOGO_WITH_COLOR
        } else {
            &LOGO_NO_COLOR
        };

        let empty_line = || (Spans::from(""), Left);

        let raw_help_lines: [(Spans, AlignLine); 12] = [
            (
                vec![
                    Span::raw("helix "),
                    Span::styled(VERSION_AND_GIT_HASH, theme.get("comment")),
                ]
                .into(),
                Center,
            ),
            empty_line(),
            (
                Span::styled(
                    "A post-modern modal text editor",
                    theme.get("ui.text").add_modifier(Modifier::ITALIC),
                )
                .into(),
                Center,
            ),
            empty_line(),
            (
                vec![
                    Span::styled(":tutor", theme.get("markup.raw")),
                    Span::styled("<enter>", theme.get("comment")),
                    Span::raw("       learn helix"),
                ]
                .into(),
                Left,
            ),
            (
                vec![
                    Span::styled(":theme", theme.get("markup.raw")),
                    Span::styled("<space><tab>", theme.get("comment")),
                    Span::raw("  choose a theme"),
                ]
                .into(),
                Left,
            ),
            (
                vec![
                    Span::styled("<space>e", theme.get("markup.raw")),
                    Span::raw("            file explorer"),
                ]
                .into(),
                Left,
            ),
            (
                vec![
                    Span::styled("<space>?", theme.get("markup.raw")),
                    Span::raw("            see all commands"),
                ]
                .into(),
                Left,
            ),
            (
                vec![
                    Span::styled(":quit", theme.get("markup.raw")),
                    Span::styled("<enter>", theme.get("comment")),
                    Span::raw("        quit helix"),
                ]
                .into(),
                Left,
            ),
            empty_line(),
            (
                vec![
                    Span::styled("docs: ", theme.get("ui.text")),
                    Span::styled("docs.helix-editor.com", theme.get("markup.link.url")),
                ]
                .into(),
                Center,
            ),
            empty_line(),
        ];

        debug_assert!(
            raw_help_lines.len() >= LOGO_STR.lines().count(),
            "help lines get chained with lines of logo. if there are not \
             enough help lines, logo will be cut off. add `empty_line()`s if necessary"
        );

        let mut help_lines = Vec::with_capacity(raw_help_lines.len());
        let mut len_of_longest_left_align = 0;
        let mut len_of_longest_center_align = 0;

        for (spans, align) in raw_help_lines {
            let width = spans.width();
            match align {
                Left => len_of_longest_left_align = len_of_longest_left_align.max(width),
                Center => len_of_longest_center_align = len_of_longest_center_align.max(width),
            }
            help_lines.push((spans, align));
        }

        let len_of_longest_left_align = len_of_longest_left_align as u16;

        // the y-coordinate where we start drawing the welcome screen
        let start_drawing_at_y =
            view.area.y + (view.area.height / 2).saturating_sub(help_lines.len() as u16 / 2);

        // x-coordinate of the center of the viewport
        let x_view_center = view.area.x + view.area.width / 2;

        // the x-coordinate where we start drawing the `AlignLine::Left` lines
        // +2 to make the text look like more balanced relative to the center of the help
        let start_drawing_left_align_at_x =
            view.area.x + (view.area.width / 2).saturating_sub(len_of_longest_left_align / 2) + 2;

        let are_any_left_aligned_lines_overflowing_x =
            (start_drawing_left_align_at_x + len_of_longest_left_align) > view.area.width;

        let are_any_center_aligned_lines_overflowing_x =
            len_of_longest_center_align as u16 > view.area.width;

        let is_help_x_overflowing =
            are_any_left_aligned_lines_overflowing_x || are_any_center_aligned_lines_overflowing_x;

        // we want `>=` so it does not get drawn over the status line
        // (essentially, it WON'T be marked as "overflowing" if the help
        // fully fits vertically in the viewport without touching the status line)
        let is_help_y_overflowing = (help_lines.len() as u16) >= view.area.height;

        // Not enough space to render the help text even without the logo. Render nothing.
        if is_help_x_overflowing || is_help_y_overflowing {
            return;
        }

        // At this point we know that there is enough vertical
        // and horizontal space to render the help text

        let width_of_help_with_logo = *LOGO_WIDTH + LOGO_LEFT_PADDING + len_of_longest_left_align;

        // If there is not enough space to show LOGO + HELP, then don't show the logo at all
        //
        // If we get here we know that there IS enough space to show just the help
        let show_logo = width_of_help_with_logo <= view.area.width;

        // Each "help" line is effectively "chained" with a line of the logo (if present).
        for (lines_drawn, (line, align)) in help_lines.iter().enumerate() {
            // Where to start drawing `AlignLine::Left` rows
            let x_start_left_help =
                start_drawing_left_align_at_x + if show_logo { *HELP_X_LOGO_OFFSET } else { 0 };

            // Where to start drawing `AlignLine::Center` rows
            let x_start_center_help = x_view_center - line.width() as u16 / 2
                + if show_logo { *HELP_X_LOGO_OFFSET } else { 0 };

            // Where to start drawing rows for the "help" section
            // Includes tips about commands. Excludes the logo.
            let x_start_help = match align {
                Left => x_start_left_help,
                Center => x_start_center_help,
            };

            let y = start_drawing_at_y + lines_drawn as u16;

            // Draw a single line of the help text
            surface.set_line(
                x_start_help,
                y,
                &tui::ratatui::to_ratatui_line(line),
                line.width() as u16,
            );

            if show_logo {
                // Draw a single line of the logo
                surface.set_line(
                    x_start_left_help - LOGO_LEFT_PADDING - *LOGO_WIDTH,
                    y,
                    &tui::ratatui::to_ratatui_line(&logo[lines_drawn]),
                    *LOGO_WIDTH,
                );
            }
        }
    }

    pub(crate) fn render_view(
        &self,
        vctx: &ViewRenderContext<'_>,
        surface: &mut CellSurface,
    ) -> RenderOutput {
        let ViewRenderContext {
            doc,
            view,
            viewport,
            is_focused,
            config,
            config_gen,
            theme,
            mode,
            syntax_loader,
            cursor_cache,
            gutter_context,
            debug_execution,
            cached_syntax,
            dirty_rows,
            seed_line_map,
            ..
        } = vctx;
        let is_focused = *is_focused;
        let inner = view.inner_area(doc);
        let area = view.area;
        let loader = syntax_loader.load();

        let view_offset = doc.view_offset(view.id);

        let render_view_start = std::time::Instant::now();
        let text_annotations_start = std::time::Instant::now();
        let text_annotations = view.text_annotations(doc, Some(theme));
        helix_view::bench::log_run_phase(
            "render_view",
            "text_annotations",
            text_annotations_start.elapsed(),
            || format!("view_id={:?}", view.id),
        );
        let mut decorations = DecorationManager::default();

        if !(is_focused && self.terminal_focused) {
            surface.set_style(
                tui::ratatui::to_ratatui_rect(area),
                tui::ratatui::to_ratatui_style(theme.get("ui.background.inactive")),
            )
        }

        if is_focused && config.cursorline {
            decorations.add_decoration(Self::cursor_line_decoration(doc, view, theme));
        }

        decorations.add_decoration(FoldDecoration::new(&text_annotations, theme));

        if is_focused && config.cursorcolumn {
            let cursorcolumn_start = std::time::Instant::now();
            Self::draw_cursor_column(doc, view, surface, theme, inner, &text_annotations);
            helix_view::bench::log_run_phase(
                "render_view",
                "cursorcolumn",
                cursorcolumn_start.elapsed(),
                || format!("view_id={:?}", view.id),
            );
        }

        // Set DAP highlights, if needed.
        if let Some(position) = debug_execution {
            let dap_line = position.line;
            let style = theme.get("ui.highlight.frameline");
            let line_decoration = move |renderer: &mut TextRenderer, pos: LinePos| {
                if pos.doc_line != dap_line {
                    return;
                }
                renderer.set_style(Rect::new(inner.x, pos.visual_line, inner.width, 1), style);
            };

            decorations.add_decoration(line_decoration);
        }

        let highlighter_start = std::time::Instant::now();
        let highlighter_input = match cached_syntax {
            Some(cache) => HighlighterInput::Cached(&cache.entries),
            None => HighlighterInput::Live(doc.viewport_syntax_highlighter(
                &loader,
                &text_annotations,
                view_offset.anchor,
                inner.height,
            )),
        };
        helix_view::bench::log_run_phase(
            "render_view",
            "highlighter_input",
            highlighter_start.elapsed(),
            || format!("view_id={:?} cached={}", view.id, cached_syntax.is_some()),
        );
        let mut overlays = Vec::new();

        let overlays_start = std::time::Instant::now();
        overlays.push(doc.viewport_overlay_highlights(
            &text_annotations,
            view_offset.anchor,
            inner.height,
        ));

        if doc
            .language_config()
            .and_then(|config| config.rainbow_brackets)
            .unwrap_or(config.rainbow_brackets)
        {
            if let Some(overlay) = doc.viewport_rainbow_highlights(
                &text_annotations,
                view_offset.anchor,
                inner.height,
                theme,
                &loader,
            ) {
                overlays.push(overlay);
            }
        }

        let viewport_range =
            doc.viewport_byte_range(&text_annotations, view_offset.anchor, inner.height);
        overlays.extend(doc.diagnostic_highlights(theme, Some(viewport_range)));

        if is_focused {
            if let Some(tabstops) = doc.tabstop_highlights(theme) {
                overlays.push(tabstops);
            }
            overlays.push(doc.selection_highlights(
                view.id,
                *mode,
                theme,
                &config.cursor_shape,
                self.terminal_focused,
                self.prompt_active,
            ));
            if let Some(overlay) = doc.matching_bracket_highlights(view.id, theme) {
                overlays.push(overlay);
            }
        }
        helix_view::bench::log_run_phase(
            "render_view",
            "overlays",
            overlays_start.elapsed(),
            || format!("view_id={:?} count={}", view.id, overlays.len()),
        );

        let gutter_overflow = view.gutter_offset(doc) == 0;
        if !gutter_overflow {
            let gutter_start = std::time::Instant::now();
            Self::render_gutter(
                gutter_context,
                doc,
                view,
                view.area,
                theme,
                is_focused & self.terminal_focused,
                &mut decorations,
            );
            helix_view::bench::log_run_phase(
                "render_view",
                "gutter",
                gutter_start.elapsed(),
                || format!("view_id={:?}", view.id),
            );
        }

        let inline_blame_start = std::time::Instant::now();
        Self::add_inline_blame(&config.inline_blame, doc, view, &mut decorations, theme);
        helix_view::bench::log_run_phase(
            "render_view",
            "inline_blame",
            inline_blame_start.elapsed(),
            || format!("view_id={:?}", view.id),
        );

        if config.welcome_screen && doc.version() == 0 && doc.is_welcome() {
            Self::draw_welcome(
                theme,
                view,
                surface,
                config.true_color || crate::true_color(),
            );
        }

        let primary_cursor = doc
            .selection(view.id)
            .primary()
            .cursor(doc.text().slice(..));
        if is_focused {
            decorations.add_decoration(text_decorations::Cursor {
                cache: cursor_cache,
                primary_cursor,
            });
        }
        let width = view.inner_width(doc);
        let config = doc.config.load();
        let enable_cursor_line = view
            .diagnostics_handler
            .show_cursorline_diagnostics(doc, view.id);
        let inline_diagnostic_config = config.inline_diagnostics.prepare(width, enable_cursor_line);
        decorations.add_decoration(InlineDiagnostics::new(
            doc,
            theme,
            primary_cursor,
            inline_diagnostic_config,
            config.end_of_line_diagnostics,
        ));

        decorations.add_decoration(PluginDecoration::new(doc, theme, view.id));

        let top_doc_line = doc
            .text()
            .char_to_line(view_offset.anchor.min(doc.text().len_chars()));
        let layout_inputs = view.layout_inputs(doc, *config_gen);
        let render_seed = seed_line_map.and_then(|line_map| {
            layout_inputs.render_seed(line_map, top_doc_line, MAX_SEED_LINE_MAP_GAP)
        });
        let render_document_start = std::time::Instant::now();
        let render_output = render_document(
            surface,
            inner,
            doc,
            view_offset,
            &text_annotations,
            highlighter_input,
            overlays,
            theme,
            decorations,
            *dirty_rows,
            render_seed,
            *seed_line_map,
        );
        helix_view::bench::log_run_phase(
            "render_view",
            "render_document",
            render_document_start.elapsed(),
            || format!("view_id={:?}", view.id),
        );

        // Draw rulers after document. Skip cells that already have content.
        let rulers_start = std::time::Instant::now();
        Self::draw_rulers(
            &config.rulers,
            &config.ruler_char,
            doc,
            view,
            inner,
            surface,
            theme,
        );
        helix_view::bench::log_run_phase("render_view", "rulers", rulers_start.elapsed(), || {
            format!("view_id={:?}", view.id)
        });

        // if we're not at the edge of the screen, draw a right border
        if viewport.right() != view.area.right() {
            let border_start = std::time::Instant::now();
            let x = area.right();
            let border_style = theme.get("ui.window");
            for y in area.top()..area.bottom() {
                {
                    if let Some(cell) = surface.cell_mut((x, y)) {
                        cell.set_symbol(tui::symbols::line::VERTICAL);
                        cell.set_style(tui::ratatui::to_ratatui_style(border_style));
                    }
                };
            }
            helix_view::bench::log_run_phase(
                "render_view",
                "right_border",
                border_start.elapsed(),
                || format!("view_id={:?} height={}", view.id, area.height),
            );
        }

        // if config.inline_diagnostics.disabled()
        //     && config.end_of_line_diagnostics == DiagnosticFilter::Disable
        // {
        //     Self::draw_diagnostics(doc, view, inner, surface, theme);
        // }

        helix_view::bench::log_run_phase(
            "render_view",
            "total",
            render_view_start.elapsed(),
            || format!("view_id={:?}", view.id),
        );

        render_output
    }

    fn add_inline_blame(
        inline_blame: &InlineBlameConfig,
        doc: &Document,
        view: &View,
        decorations: &mut DecorationManager,
        theme: &Theme,
    ) {
        const INLINE_BLAME_SCOPE: &str = "ui.virtual.inline-blame";
        // Blame is metadata — it should never compete with the
        // actual code for the reader's attention. Fall back through
        // `comment` and `ui.text.inactive` so themes that haven't
        // defined the scope still get a dim presentation. Without
        // this fallback, the default style is `ui.text` which
        // makes the blame look like code, which is the opposite of
        // what we want.
        let blame_style = theme
            .try_get(INLINE_BLAME_SCOPE)
            .or_else(|| theme.try_get("comment"))
            .or_else(|| theme.try_get("ui.text.inactive"))
            .unwrap_or_else(|| theme.get("ui.text"));
        let text = doc.text();
        match inline_blame.show {
            InlineBlameShow::Never => (),
            InlineBlameShow::CursorLine => {
                if let Some(line_blame) = doc.line_blame_at_cursor(view.id, &inline_blame.format) {
                    decorations.add_decoration(InlineBlame::new(
                        blame_style,
                        text_decorations::blame::LineBlame::OneLine(line_blame),
                    ));
                }
            }
            InlineBlameShow::AllLines => {
                let mut blame_lines = vec![None; text.len_lines()];

                for (line_idx, blame) in doc.line_blames(view, &inline_blame.format) {
                    blame_lines[line_idx] = Some(blame);
                }

                decorations.add_decoration(InlineBlame::new(
                    blame_style,
                    text_decorations::blame::LineBlame::ManyLines(blame_lines),
                ));
            }
        }
    }

    pub fn draw_rulers(
        editor_rulers: &[u16],
        ruler_char: &str,
        doc: &Document,
        view: &View,
        viewport: Rect,
        surface: &mut CellSurface,
        theme: &Theme,
    ) {
        // Base style from theme for rulers
        let base_style = theme.try_get("ui.virtual.ruler").unwrap_or_default();
        // Background style is used only for background-style rulers. If theme lacks a bg, reuse fg.
        let bg_style = if base_style.bg.is_none() {
            if let Some(fg) = base_style.fg {
                base_style.bg(fg)
            } else {
                // Fallback background to ensure visibility
                Style::default().bg(Color::Red)
            }
        } else {
            base_style
        };

        doc.ruler_columns(view, editor_rulers)
            .into_iter()
            .map(|ruler| viewport.clip_left(ruler).with_width(1))
            .for_each(|area| {
                if ruler_char.is_empty() {
                    // Background-style ruler: only apply to cells without content
                    for y in area.top()..area.bottom() {
                        // Skip cells that have non-whitespace content (like diagnostic bubbles)
                        if surface
                            .cell((area.x, y))
                            .is_some_and(|cell| cell.symbol() == " " || cell.symbol().is_empty())
                        {
                            {
                                if let Some(cell) = surface.cell_mut((area.x, y)) {
                                    cell.set_style(tui::ratatui::to_ratatui_style(bg_style));
                                }
                            };
                        }
                    }
                } else {
                    // Foreground glyph ruler: only draw on empty/space cells
                    let mut glyph_style = base_style;
                    glyph_style.bg = None;
                    if glyph_style.fg.is_none() {
                        glyph_style = glyph_style.fg(Color::Gray);
                    }
                    for y in area.top()..area.bottom() {
                        // Only draw ruler glyph on empty/space cells to avoid overwriting content
                        if surface
                            .cell((area.x, y))
                            .is_some_and(|cell| cell.symbol() == " " || cell.symbol().is_empty())
                        {
                            {
                                if let Some(cell) = surface.cell_mut((area.x, y)) {
                                    cell.set_symbol(ruler_char);
                                    cell.set_style(tui::ratatui::to_ratatui_style(glyph_style));
                                }
                            };
                        }
                    }
                }
            })
    }

    /// Render bufferline at the top from an explicit render model.
    fn draw_bufferline_model(
        &mut self,
        model: &BufferlineModel<'_>,
        viewport: Rect,
        surface: &mut CellSurface,
    ) {
        let bufferline_styles = crate::ui::design::BufferlineStyles::from_theme(model.theme);
        {
            let area = tui::ratatui::to_ratatui_rect(viewport);
            tui::ratatui::widgets::Widget::render(tui::ratatui::widgets::Clear, area, surface);
        };

        let bufferline_active = bufferline_styles.active;
        let bufferline_inactive = bufferline_styles.inactive;

        self.bufferline_info.clear();

        let icons = ICONS.load();
        let modified_accent = model
            .theme
            .try_get("ui.statusline.modified")
            .or_else(|| model.theme.try_get("warning"));

        let tabs: Vec<_> = model
            .documents
            .iter()
            .map(|doc| {
                let label = if doc.path.is_some() {
                    if let Some(icon) = icons.mime().get(doc.path, doc.language_name) {
                        format!("{} {}", icon.glyph(), doc.label)
                    } else {
                        doc.label.clone()
                    }
                } else {
                    doc.label.clone()
                };
                let base_style = if model.current_doc == doc.id {
                    bufferline_active
                } else {
                    bufferline_inactive
                };
                if doc.is_modified {
                    let dot_style = base_style.patch(modified_accent.unwrap_or(base_style));
                    Tab::cells([
                        TabCell::new(format!(" {label} ")),
                        TabCell::styled("●", dot_style),
                        TabCell::new(" "),
                    ])
                    .style(base_style)
                } else {
                    Tab::new(label).style(base_style)
                }
            })
            .collect();

        let active = model
            .documents
            .iter()
            .position(|doc| doc.id == model.current_doc)
            .unwrap_or(0);
        let chevron_style = bufferline_styles
            .inactive
            .patch(model.theme.try_get("ui.text.inactive").unwrap_or_default());
        let state = tabs_with_options(
            surface,
            viewport,
            &tabs,
            TabsOptions::new(active)
                .separator(model.separator.as_str())
                .scroll_policy(TabsScrollPolicy::CenterActive),
            TabsStyle {
                background: bufferline_styles.background,
                active: bufferline_active,
                inactive: bufferline_inactive,
                hover: Style::default(),
                badge: bufferline_inactive,
                separator: bufferline_inactive,
                overflow: chevron_style,
            },
        );

        for range in &state.tab_ranges {
            if let Some(doc) = model.documents.get(range.index) {
                self.bufferline_info.add_buffer_info(
                    doc.id,
                    viewport.x + range.visible.start..viewport.x + range.visible.end,
                );
            };
        }
    }

    pub fn render_gutter<'d>(
        gutter_context: &'d GutterContext<'d>,
        doc: &'d Document,
        view: &View,
        viewport: Rect,
        theme: &Theme,
        is_focused: bool,
        decoration_manager: &mut DecorationManager<'d>,
    ) {
        let (_, cursor_lines) = doc.cursor_lines(view.id);
        let cursors: Rc<[_]> = Rc::from(cursor_lines);

        let mut offset = 0;

        let gutter_styles = crate::ui::design::GutterStyles::from_theme(theme);

        for gutter_type in view.gutters() {
            let mut gutter = gutter_type.style(gutter_context, doc, view, theme, is_focused);
            let width = gutter_type.width(view, doc);
            // avoid lots of small allocations by reusing a text buffer for each line
            let mut text = String::with_capacity(width);
            let cursors = cursors.clone();
            let gutter_decoration = move |renderer: &mut TextRenderer, pos: LinePos| {
                // TODO handle softwrap in gutters
                let selected = cursors.contains(&pos.doc_line);
                let x = viewport.x + offset;
                let y = pos.visual_line;

                let gutter_style = match (selected, pos.first_visual_line) {
                    (false, true) => gutter_styles.base,
                    (true, true) => gutter_styles.selected,
                    (false, false) => gutter_styles.virtual_line,
                    (true, false) => gutter_styles.selected_virtual,
                };

                if let Some(style) =
                    gutter(pos.doc_line, selected, pos.first_visual_line, &mut text)
                {
                    renderer.set_stringn(x, y, &text, width, gutter_style.patch(style));
                } else {
                    renderer.set_style(
                        Rect {
                            x,
                            y,
                            width: width as u16,
                            height: 1,
                        },
                        gutter_style,
                    );
                }
                text.clear();
            };
            decoration_manager.add_decoration(gutter_decoration);

            offset += width as u16;
        }
    }

    pub fn draw_diagnostics(
        doc: &Document,
        view: &View,
        viewport: Rect,
        surface: &mut CellSurface,
        theme: &Theme,
    ) {
        use helix_core::diagnostic::Severity;
        use tui::ratatui::{
            layout::Alignment,
            widgets::{Paragraph, Widget, Wrap},
        };
        use tui::text::Text;

        let diagnostics = doc.diagnostics_at_cursor(view.id);

        let warning = theme.get("warning");
        let error = theme.get("error");
        let info = theme.get("info");
        let hint = theme.get("hint");

        let mut lines = Vec::new();
        let background_style = theme.get("ui.background");
        for diagnostic in diagnostics {
            let style = Style::reset()
                .patch(background_style)
                .patch(match diagnostic.severity {
                    Some(Severity::Error) => error,
                    Some(Severity::Warning) | None => warning,
                    Some(Severity::Info) => info,
                    Some(Severity::Hint) => hint,
                });
            let text = Text::styled(&diagnostic.message, style);
            lines.extend(text.lines);
            let code = diagnostic.code.as_ref().map(|x| match x {
                NumberOrString::Number(n) => format!("({n})"),
                NumberOrString::String(s) => format!("({s})"),
            });
            if let Some(code) = code {
                let span = Span::styled(code, style);
                lines.push(span.into());
            }
        }

        let text = Text::from(lines);
        let paragraph = Paragraph::new(tui::ratatui::to_ratatui_text(&text))
            .alignment(Alignment::Right)
            .wrap(Wrap { trim: true });
        let width = 100.min(viewport.width);
        let height = 15.min(viewport.height);
        paragraph.render(
            tui::ratatui::to_ratatui_rect(Rect::new(
                viewport.right() - width,
                viewport.y + 1,
                width,
                height,
            )),
            surface,
        );
    }

    /// Apply the highlighting on the lines where a cursor is active
    pub fn cursor_line_decoration(doc: &Document, view: &View, theme: &Theme) -> impl Decoration {
        let (primary_line, secondary_lines) = doc.cursor_lines(view.id);

        let cursorline_styles = crate::ui::design::CursorLineStyles::from_theme(theme);
        let viewport = view.area;

        move |renderer: &mut TextRenderer, pos: LinePos| {
            let area = Rect::new(viewport.x, pos.visual_line, viewport.width, 1);
            if primary_line == pos.doc_line {
                renderer.set_style(area, cursorline_styles.primary);
            } else if secondary_lines.binary_search(&pos.doc_line).is_ok() {
                renderer.set_style(area, cursorline_styles.secondary);
            }
        }
    }

    /// Apply the highlighting on the columns where a cursor is active
    pub fn draw_cursor_column(
        doc: &Document,
        view: &View,
        surface: &mut CellSurface,
        theme: &Theme,
        viewport: Rect,
        text_annotations: &TextAnnotations,
    ) {
        let text = doc.text().slice(..);

        let cursorline_styles = crate::ui::design::CursorLineStyles::from_theme(theme);
        let primary_style = cursorline_styles.column_primary;
        let secondary_style = cursorline_styles.column_secondary;

        let inner_area = view.inner_area(doc);

        let selection = doc.selection(view.id);
        let view_offset = doc.view_offset(view.id);
        let primary = selection.primary();
        let text_format = doc.text_format(viewport.width, None);
        for range in selection.iter() {
            let is_primary = primary == *range;
            let cursor = range.cursor(text);

            let Position { col, .. } =
                visual_offset_from_block(text, cursor, cursor, &text_format, text_annotations).0;

            // if the cursor is horizontally in the view
            if col >= view_offset.horizontal_offset
                && inner_area.width > (col - view_offset.horizontal_offset) as u16
            {
                let area = Rect::new(
                    inner_area.x + (col - view_offset.horizontal_offset) as u16,
                    view.area.y,
                    1,
                    view.area.height,
                );
                if is_primary {
                    surface.set_style(
                        tui::ratatui::to_ratatui_rect(area),
                        tui::ratatui::to_ratatui_style(primary_style),
                    )
                } else {
                    surface.set_style(
                        tui::ratatui::to_ratatui_rect(area),
                        tui::ratatui::to_ratatui_style(secondary_style),
                    )
                }
            }
        }
    }

    /// Unified key dispatch: resolve keymap once, route to frontend or engine.
    ///
    /// Flow:
    /// 1. Engine `pre_resolve` — count/register/dot-repeat (no keymap needed)
    /// 2. If not consumed, resolve keymap ONCE
    /// 3. If frontend result → execute frontend command
    /// 4. Otherwise → convert to `KeymapLookup`, pass to engine's `process_lookup`
    fn dispatch_key(&mut self, cx: &mut commands::Context, key: KeyEvent) {
        let dispatch_start = std::time::Instant::now();
        self.sync_context_from_engine(cx);
        let mode_before = cx.editor.mode();

        // Resolve the editing context from the focused view.
        let focus = cx.editor.focused_view_id();
        let focused_view = cx.editor.tree.get(focus);
        let view_id = focused_view.id;
        let doc_id = focused_view.doc;

        // Step 1: Engine pre-resolve (count, register, dot-repeat, escape).
        let mut engine = self.engine.take().expect("engine is always present");
        let pre_resolve_start = std::time::Instant::now();
        if let Some(result) = engine.pre_resolve(cx.editor, view_id, doc_id, &self.keymaps, key) {
            helix_view::bench::log_run_phase(
                "editor_dispatch",
                "pre_resolve",
                pre_resolve_start.elapsed(),
                || {
                    format!(
                        "key={} mode_before={:?} view_id={:?} doc_id={:?} consumed=true",
                        key.key_sequence_format(),
                        mode_before,
                        view_id,
                        doc_id
                    )
                },
            );
            self.engine = Some(engine);
            self.handle_engine_result(cx, key, result, mode_before);
            self.sync_context_from_engine(cx);
            self.publish_focused_modal_input(cx.editor);
            helix_view::bench::log_run_phase(
                "editor_dispatch",
                "total",
                dispatch_start.elapsed(),
                || format!("key={} path=pre_resolve", key.key_sequence_format()),
            );
            return;
        }
        helix_view::bench::log_run_phase(
            "editor_dispatch",
            "pre_resolve",
            pre_resolve_start.elapsed(),
            || {
                format!(
                    "key={} mode_before={:?} view_id={:?} doc_id={:?} consumed=false",
                    key.key_sequence_format(),
                    mode_before,
                    view_id,
                    doc_id
                )
            },
        );
        self.engine = Some(engine);

        // Step 2: Resolve keymap ONCE.
        let mode = cx.editor.mode();
        let keymap_start = std::time::Instant::now();
        let result = self.keymaps.get(mode, key);
        helix_view::bench::log_run_phase(
            "editor_dispatch",
            "keymap_get",
            keymap_start.elapsed(),
            || format!("key={} mode={:?}", key.key_sequence_format(), mode),
        );

        let is_frontend = crate::keymap::is_frontend_result(&result);

        // Step 3: If frontend result → execute frontend command.
        if is_frontend {
            // Reset engine pending state since frontend is taking over.
            let mut engine = self.engine.take().expect("engine is always present");
            if engine.is_pending() {
                engine.reset();
            }
            self.engine = Some(engine);

            // Update autoinfo from sticky keymap (clears stale Pending infobox).
            cx.editor.autoinfo = self.keymaps.sticky_infobox();

            let mut cmd_name: Option<&'static str> = None;
            match result {
                crate::keymap::KeymapResult::Pending(node) => {
                    cx.editor.autoinfo = Some(node.infobox());
                }
                crate::keymap::KeymapResult::Matched(cmd) => {
                    cmd_name = cmd.static_name();
                    self.execute_frontend_command(cx, &cmd);
                }
                crate::keymap::KeymapResult::MatchedSequence(cmds) => {
                    if let Some(first) = cmds.first() {
                        cmd_name = first.static_name();
                    }
                    for cmd in &cmds {
                        self.execute_frontend_command(cx, cmd);
                    }
                }
                crate::keymap::KeymapResult::NotFound
                | crate::keymap::KeymapResult::Cancelled(_)
                | crate::keymap::KeymapResult::Fallback(_, _) => unreachable!(),
            }

            self.handle_mode_change(cx, mode_before, cmd_name.or(Some("unknown")));
            self.sync_engine_from_context(cx);
            self.publish_focused_modal_input(cx.editor);
            helix_view::bench::log_run_phase(
                "editor_dispatch",
                "total",
                dispatch_start.elapsed(),
                || format!("key={} path=frontend", key.key_sequence_format()),
            );
            return;
        }

        // Step 4: Convert to engine KeymapLookup and process.
        let lookup = crate::keymap::resolve_keymap_result(&result);

        let mut engine = self.engine.take().expect("engine is always present");
        let process_start = std::time::Instant::now();
        let engine_result =
            engine.process_lookup(cx.editor, view_id, doc_id, &mut self.keymaps, key, lookup);
        helix_view::bench::log_run_phase(
            "editor_dispatch",
            "process_lookup",
            process_start.elapsed(),
            || {
                format!(
                    "key={} mode={:?} view_id={:?} doc_id={:?}",
                    key.key_sequence_format(),
                    mode,
                    view_id,
                    doc_id
                )
            },
        );
        self.engine = Some(engine);

        self.handle_engine_result(cx, key, engine_result, mode_before);
        self.sync_context_from_engine(cx);
        self.publish_focused_modal_input(cx.editor);
        helix_view::bench::log_run_phase(
            "editor_dispatch",
            "total",
            dispatch_start.elapsed(),
            || format!("key={} path=engine", key.key_sequence_format()),
        );
    }

    fn handle_engine_result(
        &mut self,
        cx: &mut commands::Context,
        key: KeyEvent,
        result: helix_view::engine::EngineResult,
        mode_before: Mode,
    ) {
        use helix_view::engine::EngineResult;

        match result {
            EngineResult::Executed => {
                self.handle_mode_change(cx, mode_before, None);
            }
            EngineResult::Pending => {
                // Engine consumed the key, waiting for more input.
            }
            EngineResult::InsertChar(ch) => {
                commands::insert::insert_char(cx, ch);
            }
            EngineResult::CancelledInsert(pending_keys) => {
                for ev in pending_keys.iter() {
                    if let Some(ch) = ev.char() {
                        commands::insert::insert_char(cx, ch);
                    }
                }
            }
            EngineResult::Unbound => {
                let is_synthetic_null = matches!(key.code, KeyCode::Null | KeyCode::Char('\0'));
                if !is_synthetic_null {
                    log::warn!("unbound key: {}", key.key_sequence_format());
                }
            }
            EngineResult::ReplayInsert {
                entry_command,
                keys,
            } => {
                self.replay_insert(cx, &entry_command, &keys);
            }
        }
    }

    /// Replay a recorded insert sequence for dot-repeat.
    fn replay_insert(
        &mut self,
        cx: &mut commands::Context,
        entry_command: &str,
        keys: &[KeyEvent],
    ) {
        if let Some(cmd) = commands::MappableCommand::builtin_commands()
            .iter()
            .find(|cmd| cmd.name() == entry_command)
        {
            let mode_before = cx.editor.mode();
            self.sync_context_from_engine(cx);
            cmd.execute(cx);
            self.sync_engine_from_context(cx);
            let mode_after = cx.editor.mode();
            if mode_after != mode_before {
                let mut event = crate::handlers::local::ModeSwitch {
                    old_mode: mode_before,
                    new_mode: mode_after,
                    cx,
                };
                crate::handlers::local::mode_switch(&mut event);
                cx.notifier.mode_switch(mode_before, mode_after);
            }
        } else {
            log::warn!("replay_insert: unknown entry command '{}'", entry_command);
            return;
        }

        for &key in keys {
            if cx.editor.mode() != Mode::Insert {
                break;
            }
            self.dispatch_key(cx, key);
        }

        if cx.editor.mode() == Mode::Insert {
            cx.editor.enter_normal_mode();
        }
    }

    fn execute_frontend_command(
        &mut self,
        cx: &mut commands::Context,
        command: &commands::MappableCommand,
    ) {
        self.sync_context_from_engine(cx);
        command.execute(cx);
        self.sync_engine_from_context(cx);

        if let Some(static_command) = commands::MappableCommand::builtin_commands()
            .iter()
            .find(|candidate| candidate.name() == command.name())
        {
            crate::handlers::local::post_command(static_command, cx);
        }
    }

    /// Track mode changes, fire events, and manage insert recording for dot-repeat.
    ///
    /// `command_name` is the name of the command that triggered the mode change,
    /// used as the entry command for insert recording. Pass `None` when the command
    /// name is not known (e.g., engine-dispatched mode changes).
    fn handle_mode_change(
        &mut self,
        cx: &mut commands::Context,
        mode_before: Mode,
        command_name: Option<&str>,
    ) {
        let mode_after = cx.editor.mode();
        if mode_after != mode_before {
            let mut event = crate::handlers::local::ModeSwitch {
                old_mode: mode_before,
                new_mode: mode_after,
                cx,
            };
            crate::handlers::local::mode_switch(&mut event);
            cx.notifier.mode_switch(mode_before, mode_after);

            let engine = self.engine.as_mut().expect("engine is always present");

            if mode_after == Mode::Insert && mode_before != Mode::Insert {
                // Entering insert mode — start recording for dot-repeat.
                let entry = command_name
                    .map(|n| std::borrow::Cow::Owned(n.to_string()))
                    .unwrap_or(std::borrow::Cow::Borrowed("insert_mode"));
                engine.begin_insert_recording(entry);
            } else if mode_before == Mode::Insert && mode_after != Mode::Insert {
                // Leaving insert mode — finalize recording.
                engine.end_insert_recording();
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn set_completion(
        &mut self,
        editor: &mut Editor,
        items: Vec<CompletionItem>,
        trigger_offset: usize,
        size: Rect,
        ingress: crate::runtime::RuntimeIngress,
    ) -> Option<Rect> {
        let mut completion = Completion::new(
            editor,
            items,
            trigger_offset,
            crate::handlers::completion::ResolveRuntime::new(editor.runtime()),
            ingress,
        );

        if completion.is_empty() {
            // skip if we got no completion results
            return None;
        }

        let area = completion.area(size, editor);
        editor.last_completion = Some(CompleteAction::Triggered);

        // TODO : propagate required size on resize to completion too
        self.completion = Some(completion);
        Some(area)
    }

    pub fn clear_completion(&mut self, editor: &mut Editor) -> Option<OnKeyCallback> {
        if let Some(ref completion) = self.completion {
            completion.remove_model_layer(editor);
        }
        self.completion = None;
        let mut on_next_key: Option<OnKeyCallback> = None;
        editor.clear_completion_requests();
        if let Some(last_completion) = editor.last_completion.take() {
            match last_completion {
                CompleteAction::Triggered => (),
                CompleteAction::Applied {
                    trigger_offset: _,
                    changes: _,
                    placeholder,
                } => {
                    on_next_key = placeholder.then_some(Box::new(|cx, key| {
                        if let Some(c) = key.char() {
                            let (view_id, doc) = focused!(cx.editor);
                            if let Some(snippet) = doc.active_snippet() {
                                doc.apply(&snippet.delete_placeholder(doc.text()), view_id);
                            }
                            commands::insert::insert_char(cx, c);
                        }
                    }))
                }
                CompleteAction::Selected { savepoint } => {
                    let (view_id, doc) = focused!(editor);
                    let view = view_mut!(editor, view_id);
                    doc.restore(view, &savepoint, false);
                }
            }
        }
        on_next_key
    }

    pub fn handle_idle_timeout(&mut self, cx: &mut commands::Context) -> EventResult {
        commands::compute_inlay_hints_for_all_views(cx.editor, cx.ingress.clone());

        EventResult::Ignored(None)
    }
}

impl EditorView {
    /// must be called whenever the editor processed input that
    /// is not a `KeyEvent`. In these cases any pending keys/on next
    /// key callbacks must be canceled.
    fn handle_non_key_input(&mut self, cxt: &mut commands::Context) {
        cxt.editor.status_msg = None;
        cxt.reset_idle_timer();
        // HACKS: create a fake key event that will never trigger any actual map
        // and therefore simply acts as "dismiss"
        let null_key_event = KeyEvent {
            code: KeyCode::Null,
            modifiers: KeyModifiers::empty(),
        };
        // dismiss any pending keys
        if let Some((on_next_key, _)) = self.on_next_key.take() {
            on_next_key(cxt, null_key_event);
        }
        // Feed the null key through the engine to dismiss any pending keymap state
        self.dispatch_key(cxt, null_key_event);
        self.pseudo_pending.clear();
    }

    fn handle_mouse_event(
        &mut self,
        event: &MouseEvent,
        cxt: &mut commands::Context,
    ) -> EventResult {
        if event.kind != MouseEventKind::Moved {
            self.handle_non_key_input(cxt)
        }

        let config = cxt.editor.config();
        let MouseEvent {
            kind,
            row,
            column,
            modifiers,
            ..
        } = *event;

        let pos_and_view = |editor: &Editor, row, column, ignore_virtual_text| {
            editor.tree.views().find_map(|(view, _focus)| {
                view.pos_at_screen_coords(
                    &editor.documents[&view.doc],
                    row,
                    column,
                    ignore_virtual_text,
                )
                .map(|pos| (pos, view.id))
            })
        };

        let gutter_coords_and_view = |editor: &Editor, row, column| {
            editor.tree.views().find_map(|(view, _focus)| {
                view.gutter_coords_at_screen_coords(row, column)
                    .map(|coords| (coords, view.id))
            })
        };

        match kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let editor = &mut cxt.editor;

                let config = editor.config();
                let bufferline_visible = match config.bufferline.render_mode {
                    helix_view::editor::BufferLineRenderMode::Always => true,
                    helix_view::editor::BufferLineRenderMode::Multiple => {
                        editor.has_multiple_documents()
                    }
                    _ => false,
                };
                if bufferline_visible && row == 0 {
                    if let Some(buffer_info) = self.bufferline_info.get_clicked_buffer(column) {
                        editor.switch(buffer_info.document_id, helix_view::editor::Action::Replace);
                    }

                    return EventResult::Consumed(None);
                }

                if let Some((pos, view_id)) = pos_and_view(editor, row, column, true) {
                    editor.focus(view_id);

                    let prev_view_id = view!(editor).id;
                    let doc = doc_mut!(editor, &view!(editor, view_id).doc);

                    if modifiers == KeyModifiers::ALT {
                        let selection = doc.selection(view_id).clone();
                        doc.set_selection(view_id, selection.push(Range::point(pos)));
                    } else if editor.mode == Mode::Select {
                        // Discards non-primary selections for consistent UX with normal mode
                        let primary = doc.selection(view_id).primary().put_cursor(
                            doc.text().slice(..),
                            pos,
                            true,
                        );
                        editor.mouse_down_range = Some(primary);
                        doc.set_selection(view_id, Selection::single(primary.anchor, primary.head));
                    } else {
                        doc.set_selection(view_id, Selection::point(pos));
                    }

                    if view_id != prev_view_id {
                        self.clear_completion(editor);
                    }

                    editor.ensure_cursor_in_view(view_id);

                    return EventResult::Consumed(None);
                }

                if let Some((coords, view_id)) = gutter_coords_and_view(editor, row, column) {
                    editor.focus(view_id);

                    let (view_id, doc) = focused!(cxt.editor);
                    let view = view!(cxt.editor, view_id);

                    let path = match doc.path() {
                        Some(path) => path.clone(),
                        None => return EventResult::Ignored(None),
                    };

                    if let Some(char_idx) =
                        view.pos_at_visual_coords(doc, coords.row as u16, coords.col as u16, true)
                    {
                        let line = doc.text().char_to_line(char_idx);
                        commands::dap_toggle_breakpoint_impl(cxt, path, line);
                        return EventResult::Consumed(None);
                    }
                }

                EventResult::Ignored(None)
            }

            MouseEventKind::Drag(MouseButton::Left) => {
                let (view_id, doc) = focused!(cxt.editor);
                let view = view!(cxt.editor, view_id);

                let pos = match view.pos_at_screen_coords(doc, row, column, true) {
                    Some(pos) => pos,
                    None => return EventResult::Ignored(None),
                };

                let mut selection = doc.selection(view_id).clone();
                let primary = selection.primary_mut();
                *primary = primary.put_cursor(doc.text().slice(..), pos, true);
                doc.set_selection(view_id, selection);
                cxt.editor.ensure_cursor_in_view(view_id);
                EventResult::Consumed(None)
            }

            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let direction = match event.kind {
                    MouseEventKind::ScrollUp => Direction::Backward,
                    MouseEventKind::ScrollDown => Direction::Forward,
                    _ => unreachable!(),
                };

                let scrolled_view = match pos_and_view(cxt.editor, row, column, false) {
                    Some((_, view_id)) => view_id,
                    None => return EventResult::Ignored(None),
                };

                let offset = config.scroll_lines.unsigned_abs();
                cxt.editor.with_temporary_focus(scrolled_view, |editor| {
                    let mut scroll_cx = crate::commands::Context {
                        register: cxt.register,
                        count: cxt.count,
                        editor,
                        registry: cxt.registry.clone(),
                        notifier: cxt.notifier.clone(),
                        callback: std::mem::take(&mut cxt.callback),
                        on_next_key_callback: cxt.on_next_key_callback.take(),
                        exit_tasks: cxt.exit_tasks,
                        exit_task_work: cxt.exit_task_work.clone(),
                        ingress: cxt.ingress.clone(),
                        redraw: cxt.redraw.clone(),
                        idle_reset: cxt.idle_reset.clone(),
                        plugin_manager: cxt.plugin_manager.clone(),
                    };
                    commands::scroll(&mut scroll_cx, offset, direction, false);
                    cxt.callback = scroll_cx.callback;
                    cxt.on_next_key_callback = scroll_cx.on_next_key_callback;
                });

                EventResult::Consumed(None)
            }

            MouseEventKind::Up(MouseButton::Left) => {
                if !config.middle_click_paste {
                    return EventResult::Ignored(None);
                }

                let (view_id, doc) = focused!(cxt.editor);

                let should_yank = match cxt.editor.mouse_down_range.take() {
                    Some(down_range) => doc.selection(view_id).primary() != down_range,
                    None => {
                        // This should not happen under normal cases. We fall back to the original
                        // behavior of yanking on non-single-char selections.
                        doc.selection(view_id)
                            .primary()
                            .slice(doc.text().slice(..))
                            .len_chars()
                            > 1
                    }
                };

                if should_yank {
                    commands::MappableCommand::builtin_named(
                        "yank_main_selection_to_primary_clipboard",
                    )
                    .execute(cxt);
                    EventResult::Consumed(None)
                } else {
                    EventResult::Ignored(None)
                }
            }

            MouseEventKind::Up(MouseButton::Right) => {
                if let Some((pos, view_id)) = gutter_coords_and_view(cxt.editor, row, column) {
                    cxt.editor.focus(view_id);

                    if let Some((pos, _)) = pos_and_view(cxt.editor, row, column, true) {
                        focused!(cxt.editor)
                            .1
                            .set_selection(view_id, Selection::point(pos));
                    } else {
                        let (view_id, doc) = focused!(cxt.editor);
                        let view = view!(cxt.editor, view_id);

                        if let Some(pos) = view.pos_at_visual_coords(doc, pos.row as u16, 0, true) {
                            doc.set_selection(view_id, Selection::point(pos));
                            match modifiers {
                                KeyModifiers::ALT => {
                                    commands::MappableCommand::builtin_named("dap_edit_log")
                                        .execute(cxt)
                                }
                                _ => commands::MappableCommand::builtin_named("dap_edit_condition")
                                    .execute(cxt),
                            };
                        }
                    }

                    cxt.editor.ensure_cursor_in_view(view_id);
                    return EventResult::Consumed(None);
                }
                EventResult::Ignored(None)
            }

            MouseEventKind::Up(MouseButton::Middle) => {
                let editor = &mut cxt.editor;
                if !config.middle_click_paste {
                    return EventResult::Ignored(None);
                }

                if modifiers == KeyModifiers::ALT {
                    commands::MappableCommand::builtin_named(
                        "replace_selections_with_primary_clipboard",
                    )
                    .execute(cxt);

                    return EventResult::Consumed(None);
                }

                if let Some((pos, view_id)) = pos_and_view(editor, row, column, true) {
                    let doc = doc_mut!(editor, &view!(editor, view_id).doc);
                    doc.set_selection(view_id, Selection::point(pos));
                    cxt.editor.focus(view_id);
                    commands::MappableCommand::named("paste_primary_clipboard_before")
                        .expect("engine command must exist")
                        .execute(cxt);

                    return EventResult::Consumed(None);
                }

                EventResult::Ignored(None)
            }

            _ => EventResult::Ignored(None),
        }
    }
    fn on_next_key(
        &mut self,
        kind: OnKeyCallbackKind,
        ctx: &mut commands::Context,
        event: KeyEvent,
    ) -> bool {
        if let Some((on_next_key, kind_)) = self.on_next_key.take() {
            if kind == kind_ {
                on_next_key(ctx, event);
                true
            } else {
                self.on_next_key = Some((on_next_key, kind_));
                false
            }
        } else {
            false
        }
    }
}

impl Component for EditorView {
    fn handle_event(
        &mut self,
        event: &Event,
        context: &mut crate::compositor::Context,
    ) -> EventResult {
        self.publish_focused_modal_input(context.editor);
        let mut cx = commands::Context {
            editor: context.editor,
            registry: self.registry.clone(),
            notifier: context.notifier.clone(),
            count: self.engine_input_state().count,
            register: self.engine_input_state().selected_register,
            callback: Vec::new(),
            on_next_key_callback: None,
            exit_tasks: context.exit_tasks,
            exit_task_work: context.exit_task_work.clone(),
            ingress: context.ingress.clone(),
            redraw: context.redraw.clone(),
            idle_reset: context.idle_reset.clone(),
            plugin_manager: context.plugin_manager.clone(),
        };

        match event {
            Event::Paste(contents) => {
                self.handle_non_key_input(&mut cx);
                commands::paste_bracketed_value(&mut cx, contents.clone());
                cx.count = None;
                self.sync_engine_from_context(&mut cx);

                let config = cx.editor.config();
                let mode = cx.editor.mode();
                let (view_id, doc) = focused!(cx.editor);
                let view = view_mut!(cx.editor, view_id);
                view.ensure_cursor_in_view(doc, config.scrolloff);

                // Store a history state if not in insert mode. Otherwise wait till we exit insert
                // to include any edits to the paste in the history state.
                if mode != Mode::Insert {
                    doc.append_changes_to_history(view);
                }

                EventResult::Consumed(None)
            }
            Event::Resize(_width, _height) => {
                // Ignore this event, we handle resizing just before rendering to screen.
                // Handling it here but not re-rendering will cause flashing
                self.view_cache.clear();
                EventResult::Consumed(None)
            }
            Event::Key(key) => {
                let key = *key;
                let key_dispatch_start = std::time::Instant::now();
                cx.reset_idle_timer();
                // Key is already canonicalized by the compositor.

                // clear status
                cx.editor.status_msg = None;

                let mode = cx.editor.mode();

                self.sync_context_from_engine(&mut cx);
                if !self.on_next_key(OnKeyCallbackKind::PseudoPending, &mut cx, key) {
                    if mode == Mode::Insert {
                        // Let completion swallow the event first
                        let mut consumed = false;
                        if let Some(completion) = &mut self.completion {
                            let res = {
                                let mut completion_cx = cx.compositor_context();
                                if let EventResult::Consumed(callback) =
                                    completion.handle_event(event, &mut completion_cx)
                                {
                                    consumed = true;
                                    Some(callback)
                                } else if let EventResult::Consumed(callback) = completion
                                    .handle_event(&Event::Key(key!(Enter)), &mut completion_cx)
                                {
                                    Some(callback)
                                } else {
                                    None
                                }
                            };
                            if let Some(callback) = res {
                                if callback.is_some() {
                                    if let Some(cb) = self.clear_completion(cx.editor) {
                                        if consumed {
                                            cx.on_next_key_callback =
                                                Some((cb, OnKeyCallbackKind::Fallback))
                                        } else {
                                            self.on_next_key =
                                                Some((cb, OnKeyCallbackKind::Fallback));
                                        }
                                    }
                                }
                            }
                        }
                        if !consumed {
                            let dispatch_start = std::time::Instant::now();
                            self.dispatch_key(&mut cx, key);
                            helix_view::bench::log_run_phase(
                                "editor_key",
                                "dispatch_key",
                                dispatch_start.elapsed(),
                                || format!("key={} mode={:?}", key.key_sequence_format(), mode),
                            );
                        }
                    } else {
                        let dispatch_start = std::time::Instant::now();
                        self.dispatch_key(&mut cx, key);
                        helix_view::bench::log_run_phase(
                            "editor_key",
                            "dispatch_key",
                            dispatch_start.elapsed(),
                            || format!("key={} mode={:?}", key.key_sequence_format(), mode),
                        );
                    }
                } else {
                    self.sync_engine_from_context(&mut cx);
                }

                self.on_next_key = cx.on_next_key_callback.take();
                match self.on_next_key {
                    Some((_, OnKeyCallbackKind::PseudoPending)) => self.pseudo_pending.push(key),
                    _ => self.pseudo_pending.clear(),
                }

                // appease borrowck
                let callbacks = take(&mut cx.callback);

                // if the command consumed the last view, skip the render.
                // on the next loop cycle the Application will then terminate.
                if cx.editor.should_close() {
                    return EventResult::Ignored(None);
                }

                let config = cx.editor.config();
                let mode = cx.editor.mode();
                let (view_id, doc) = focused!(cx.editor);
                let view = view_mut!(cx.editor, view_id);

                let ensure_cursor_start = std::time::Instant::now();
                view.ensure_cursor_in_view(doc, config.scrolloff);
                helix_view::bench::log_run_phase(
                    "editor_key",
                    "ensure_cursor_in_view",
                    ensure_cursor_start.elapsed(),
                    || {
                        format!(
                            "key={} mode={:?} view_id={:?} doc_id={:?}",
                            key.key_sequence_format(),
                            mode,
                            view_id,
                            doc.id()
                        )
                    },
                );

                // Store a history state if not in insert mode. This also takes care of
                // committing changes when leaving insert mode.
                if mode != Mode::Insert {
                    let history_start = std::time::Instant::now();
                    doc.append_changes_to_history(view);
                    helix_view::bench::log_run_phase(
                        "editor_key",
                        "append_changes_to_history",
                        history_start.elapsed(),
                        || {
                            format!(
                                "key={} mode={:?} view_id={:?} doc_id={:?}",
                                key.key_sequence_format(),
                                mode,
                                view_id,
                                doc.id()
                            )
                        },
                    );
                }
                let callback = if callbacks.is_empty() {
                    None
                } else {
                    Some(crate::compositor::PostAction::Batch(callbacks))
                };

                helix_view::bench::log_run_phase(
                    "editor_key",
                    "total",
                    key_dispatch_start.elapsed(),
                    || format!("key={} mode={:?}", key.key_sequence_format(), mode),
                );

                EventResult::Consumed(callback)
            }

            Event::Mouse(event) => self.handle_mouse_event(event, &mut cx),
            Event::IdleTimeout => self.handle_idle_timeout(&mut cx),
            Event::FocusGained => {
                self.terminal_focused = true;
                self.view_cache.clear();
                EventResult::Consumed(None)
            }
            Event::FocusLost => {
                if context.editor.config().auto_save.focus_lost {
                    let options = commands::WriteAllOptions {
                        policy: helix_view::editor::SavePolicy::Safe,
                        write_scratch: false,
                        auto_format: false,
                    };
                    if let Err(e) = commands::typed::write_all_impl(context, options) {
                        context.editor.set_error(format!("{}", e));
                    }
                }
                self.terminal_focused = false;
                self.view_cache.clear();
                EventResult::Consumed(None)
            }
        }
    }

    fn sync(&mut self, editor: &mut Editor) {
        if editor.model.focus == FocusTarget::Editor {
            self.publish_focused_modal_input(editor);
        }

        // Pre-resolve completion item so the render phase doesn't need &mut Editor.
        if let Some(completion) = self.completion.as_mut() {
            completion.resolve_selected_item(editor);
        }
    }

    fn layout_role(&self) -> crate::compositor::LayoutRole {
        crate::compositor::LayoutRole::Fill
    }

    fn render(&mut self, area: Rect, surface: &mut CellSurface, cx: &RenderContext) {
        static FRAME_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let frame_num = FRAME_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let render_start = std::time::Instant::now();
        let clear_start = std::time::Instant::now();
        surface.set_style(
            tui::ratatui::to_ratatui_rect(area),
            tui::ratatui::to_ratatui_style(cx.style("ui.background")),
        );
        helix_view::bench::log_run_phase(
            "editor_render",
            "clear_background_ratatui",
            clear_start.elapsed(),
            || format!("area={}x{}", area.width, area.height),
        );
        let config = cx.config();

        use helix_view::editor::BufferLineRenderMode;
        let use_bufferline = match config.bufferline.render_mode {
            BufferLineRenderMode::Always => true,
            BufferLineRenderMode::Multiple if cx.has_multiple_documents() => true,
            _ => false,
        };

        if use_bufferline {
            let bufferline_start = std::time::Instant::now();
            let bufferline_area = area.with_height(1);
            let mut output = crate::render::RenderOutput::new(bufferline_area);
            let model =
                BufferlineModel::from_render_context(cx, config.bufferline.separator.as_str());
            self.draw_bufferline_model(&model, bufferline_area, output.surface_mut());
            let prepared = PreparedRender::ready(output);
            self.chrome_cache.compose(prepared, surface);
            helix_view::bench::log_run_phase(
                "editor_render",
                "bufferline_ratatui",
                bufferline_start.elapsed(),
                || format!("area={}x{}", area.width, 1),
            );
        }

        {
            let active: std::collections::HashSet<ViewId> = cx.views().map(|(v, _)| v.id).collect();
            self.view_cache.retain_active_views(&active);
            self.chrome_cache.retain(|id| {
                active
                    .iter()
                    .any(|view_id| statusline::cache_id(*view_id) == id)
            });
        }

        let mut view_cache = std::mem::take(&mut self.view_cache);
        self.render_views(area, surface, cx, &mut view_cache, frame_num, render_start);
        self.view_cache = view_cache;

        {
            let statusline_start = std::time::Instant::now();
            let batch: Vec<PreparedRender> = cx
                .views()
                .map(|(view, is_focused)| {
                    let doc = cx.document(view.doc).unwrap();
                    self.prepare_statusline(cx, doc, view, is_focused)
                })
                .collect();
            let count = batch.len();
            self.chrome_cache.compose_batch(batch, surface);
            helix_view::bench::log_run_phase(
                "editor_render",
                "statusline_batch_ratatui",
                statusline_start.elapsed(),
                || format!("count={}", count),
            );
        }

        self.view_cache.log_and_reset_stats();

        let key_width = 15u16;
        // status_msg is now rendered by the compositor in the reserved
        // global status row (full terminal width, below all chrome), so
        // EditorView no longer paints it inside its own area.
        let status_msg_width = 0u16;

        if area.width.saturating_sub(status_msg_width) > key_width {
            let pending_start = std::time::Instant::now();
            let mut disp = String::new();
            if let Some(count) = self.engine_input_state().count {
                disp.push_str(&count.to_string())
            }
            if let Some(ref engine) = self.engine {
                let pending = engine.pending_display();
                if !pending.is_empty() {
                    disp.push_str(pending);
                }
            }
            for key in self.keymaps.pending() {
                disp.push_str(&key.key_sequence_format());
            }
            for key in &self.pseudo_pending {
                disp.push_str(&key.key_sequence_format());
            }
            let style = cx.style("ui.text");
            let macro_width = if cx.macro_recording_register().is_some() {
                3
            } else {
                0
            };
            surface.set_string(
                area.x + area.width.saturating_sub(key_width + macro_width),
                area.y + area.height.saturating_sub(1),
                disp.get(disp.len().saturating_sub(key_width as usize)..)
                    .unwrap_or(&disp),
                tui::ratatui::to_ratatui_style(style),
            );
            if let Some(reg) = cx.macro_recording_register() {
                let disp = format!("[{}]", reg);
                let style = style
                    .fg(helix_view::graphics::Color::Yellow)
                    .add_modifier(Modifier::BOLD);
                surface.set_string(
                    area.x + area.width.saturating_sub(3),
                    area.y + area.height.saturating_sub(1),
                    &disp,
                    tui::ratatui::to_ratatui_style(style),
                );
            }
            helix_view::bench::log_run_phase(
                "editor_render",
                "pending_keys_ratatui",
                pending_start.elapsed(),
                || format!("display_width={}", disp.len()),
            );
        }

        {
            let chrome_start = std::time::Instant::now();

            if let Some(completion) = self.completion.as_mut() {
                let prepared = completion.prepare_render(area, cx);
                self.chrome_cache.compose(prepared, surface);
            }
            if let Some(prepared) = self.notification_popup.prepare_snapshot(area, cx) {
                self.chrome_cache.compose(prepared, surface);
            }
            helix_view::bench::log_run_phase(
                "editor_render",
                "chrome_batch_ratatui",
                chrome_start.elapsed(),
                || format!("area={}x{}", area.width, area.height),
            );
        }
        helix_view::bench::log_run_phase(
            "editor_render",
            "final_total_ratatui",
            render_start.elapsed(),
            || format!("area={}x{} frame={}", area.width, area.height, frame_num),
        );
    }

    fn cursor(&self, _area: Rect, editor: &Editor) -> (Option<Position>, CursorKind) {
        if editor.model.focus != FocusTarget::Editor {
            return (None, CursorKind::Hidden);
        }

        let (pos, kind) = editor.cursor();
        if self.terminal_focused {
            (pos, kind)
        } else {
            // use underline cursor when terminal loses focus for visibility
            (pos, CursorKind::Underline)
        }
    }
}

#[derive(Debug, Default)]
struct BufferLineInfo {
    visible_buffers: Vec<BufferInfo>,
}

impl BufferLineInfo {
    fn clear(&mut self) {
        self.visible_buffers.clear();
    }

    fn add_buffer_info(&mut self, document_id: DocumentId, columns: std::ops::Range<u16>) {
        self.visible_buffers.push(BufferInfo {
            document_id,
            columns,
        });
    }

    fn get_clicked_buffer(&self, column: u16) -> Option<&BufferInfo> {
        self.visible_buffers
            .iter()
            .find(|cell| cell.columns.contains(&column))
    }
}

#[derive(Debug)]
struct BufferInfo {
    document_id: DocumentId,
    // The bufferline column span used to show the document name
    columns: std::ops::Range<u16>,
}

// Key canonicalization (SHIFT stripping from Char keys) is now done in
// the compositor's handle_event, so all components receive canonical keys.

#[cfg(test)]
mod tests {
    use super::{BufferlineDocument, BufferlineModel, EditorView, ViewRenderContext};
    use crate::compositor::Component;
    use crate::handlers::Handlers;
    use crate::keymap::Keymaps;
    use crate::render::CellSurface;
    use arc_swap::ArcSwap;
    use helix_core::Rope;
    use helix_loader::runtime_dirs;
    use helix_modal::{helix::HelixEngine, CommandRegistry};
    use helix_view::graphics::{CursorKind, Rect};
    use helix_view::gutter::GutterContext;
    use helix_view::model::{FocusTarget, PanelSide, PanelSize, TreePanelModel};
    use helix_view::theme;
    use helix_view::view::{
        LayoutSnapshot, LineMap, ViewLayoutInputs, ViewPosition, VisualLineInfo,
    };
    use helix_view::{
        editor::{Action, Config, Editor},
        Document, DocumentId, View,
    };
    use std::borrow::Cow;
    use std::path::Path;
    use std::sync::Arc;

    fn layout_inputs(
        doc_version: i32,
        annotation: helix_view::presentation_state::AnnotationSnapshot,
        width: u16,
    ) -> ViewLayoutInputs {
        ViewLayoutInputs {
            doc_id: DocumentId::default(),
            doc_version,
            view_position: ViewPosition::default(),
            area: Rect::new(0, 0, width, 10),
            config_gen: 1,
            annotation,
        }
    }

    fn layout_snapshot(line_map: LineMap, horizontal_offset: usize) -> LayoutSnapshot {
        let mut inputs = layout_inputs(
            1,
            helix_view::presentation_state::AnnotationSnapshot::new(helix_view::Revision::default()),
            120,
        );
        inputs.view_position.horizontal_offset = horizontal_offset;
        LayoutSnapshot::new(inputs, line_map)
    }

    fn test_editor_with_text(text: &str) -> (Editor, helix_view::ViewId, DocumentId) {
        let theme_loader = theme::Loader::new(runtime_dirs());
        let syn_loader = helix_core::config::default_lang_loader();
        let config = Arc::new(ArcSwap::from_pointee(Config::default()));
        let mut editor = Editor::new(
            Rect::new(0, 0, 80, 24),
            Arc::new(theme_loader),
            Arc::new(ArcSwap::from_pointee(syn_loader)),
            Arc::new(arc_swap::access::Map::new(config, |cfg: &Config| cfg)),
            helix_runtime::test::runtime(),
            Handlers::dummy(),
        );
        let doc = Document::from(
            Rope::from(text),
            None,
            editor.config.clone(),
            editor.syn_loader.clone(),
        );
        let doc_id = editor.new_file_from_document(Action::VerticalSplit, doc);
        let view_id = editor.focused_view_id();
        (editor, view_id, doc_id)
    }

    fn test_editor_view() -> EditorView {
        let registry = Arc::new(CommandRegistry::builtins());
        EditorView::new(
            Keymaps::default(),
            Box::new(HelixEngine::new(registry.clone())),
            registry,
        )
    }

    #[test]
    fn editor_view_cursor_is_hidden_when_model_focus_is_not_editor() {
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let (mut editor, _, _) = test_editor_with_text("alpha\n");
            let view = test_editor_view();
            let panel_id = editor.model.insert_panel(
                "Files",
                Box::new(TreePanelModel::default()),
                PanelSide::Left,
                PanelSize::fixed(34),
            );
            editor.model.focus = FocusTarget::Panel(panel_id);

            let (pos, kind) = view.cursor(Rect::new(0, 0, 80, 24), &editor);

            assert_eq!(pos, None);
            assert_eq!(kind, CursorKind::Hidden);
        });
    }

    fn giant_multiline_fixture(lines: usize, bytes_per_line: usize) -> String {
        (0..lines)
            .map(|idx| {
                char::from(b'a' + (idx % 26) as u8)
                    .to_string()
                    .repeat(bytes_per_line)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn render_seed_prefers_nearest_visible_checkpoint() {
        let line_map = LineMap {
            lines: Arc::from(vec![VisualLineInfo {
                visual_row: 0,
                doc_line: 7,
                char_range_start: 0,
                char_range_end: 200,
                visible_char_start: 10,
                visible_col_start: 10,
                visible_char_last: 90,
                visible_col_last: 90,
                horizontal_checkpoints: Default::default(),
            }]),
        };
        let layout = layout_snapshot(line_map, 100);

        assert_eq!(
            layout.render_seed(7, 4_096).map(|seed| (
                seed.doc_line,
                seed.char_idx,
                seed.visual_col
            )),
            Some((7, 90, 90))
        );
    }

    #[test]
    fn render_seed_ignores_future_and_mismatched_rows() {
        let line_map = LineMap {
            lines: Arc::from(vec![
                VisualLineInfo {
                    visual_row: 1,
                    doc_line: 7,
                    char_range_start: 0,
                    char_range_end: 50,
                    visible_char_start: 10,
                    visible_col_start: 10,
                    visible_char_last: 20,
                    visible_col_last: 20,
                    horizontal_checkpoints: Default::default(),
                },
                VisualLineInfo {
                    visual_row: 0,
                    doc_line: 8,
                    char_range_start: 50,
                    char_range_end: 100,
                    visible_char_start: 60,
                    visible_col_start: 60,
                    visible_char_last: 80,
                    visible_col_last: 120,
                    horizontal_checkpoints: Default::default(),
                },
            ]),
        };
        let layout = layout_snapshot(line_map, 100);

        assert_eq!(
            layout.render_seed(8, 4_096).map(|seed| (
                seed.doc_line,
                seed.char_idx,
                seed.visual_col
            )),
            Some((8, 60, 60))
        );
        assert!(layout.render_seed(9, 4_096).is_none());
    }

    #[test]
    fn render_seed_ignores_far_cached_checkpoint() {
        let line_map = LineMap {
            lines: Arc::from(vec![VisualLineInfo {
                visual_row: 0,
                doc_line: 7,
                char_range_start: 0,
                char_range_end: 300_000,
                visible_char_start: 143_200,
                visible_col_start: 143_200,
                visible_char_last: 143_237,
                visible_col_last: 143_237,
                horizontal_checkpoints: Default::default(),
            }]),
        };
        let far_layout = layout_snapshot(line_map.clone(), 290_989);
        let near_layout = layout_snapshot(line_map, 143_296);

        assert!(far_layout.render_seed(7, 4_096).is_none());
        assert_eq!(
            near_layout.render_seed(7, 4_096).map(|seed| (
                seed.doc_line,
                seed.char_idx,
                seed.visual_col
            )),
            Some((7, 143_237, 143_237))
        );
    }

    #[test]
    fn render_view_can_target_ratatui_surface() {
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let (mut editor, view_id, doc_id) = test_editor_with_text("alpha\nbeta\n");
            let area = Rect::new(0, 0, 80, 24);
            editor.resize(area);

            let editor_view = test_editor_view();
            let doc = editor.document(doc_id).expect("document");
            let view = view!(editor, view_id);
            let config = editor.config();
            let vctx = ViewRenderContext {
                doc,
                view,
                viewport: area,
                is_focused: true,
                config: &config,
                config_gen: editor.config_gen,
                theme: &editor.theme,
                mode: editor.mode(),
                syntax_loader: &editor.syn_loader,
                cursor_cache: &editor.cursor_cache,
                gutter_context: GutterContext {
                    mode: editor.mode(),
                    line_number: config.line_number,
                    wrap_indicator: config
                        .soft_wrap
                        .wrap_indicator
                        .as_deref()
                        .map_or(Cow::Borrowed("↪"), Cow::Borrowed),
                    breakpoints: doc
                        .path()
                        .and_then(|path| editor.breakpoints.get(path))
                        .map(Vec::as_slice),
                    debug_execution: None,
                },
                debug_execution: None,
                cached_syntax: None,
                dirty_rows: None,
                seed_line_map: None,
            };
            let mut surface = CellSurface::empty(tui::ratatui::layout::Rect::new(0, 0, 80, 24));

            let output = editor_view.render_view(&vctx, &mut surface);

            assert!(!output.line_map.lines.is_empty());
            assert!(surface.content.iter().any(|cell| cell.symbol() == "a"));
        });
    }

    #[test]
    fn seed_line_map_reuse_requires_stable_text_layout_inputs() {
        let previous = layout_inputs(
            5,
            helix_view::presentation_state::AnnotationSnapshot::new(helix_view::Revision::from(7)),
            120,
        );
        let same_text = layout_inputs(
            5,
            helix_view::presentation_state::AnnotationSnapshot::new(helix_view::Revision::from(7)),
            120,
        );
        let changed_doc = layout_inputs(
            6,
            helix_view::presentation_state::AnnotationSnapshot::new(helix_view::Revision::from(7)),
            120,
        );
        let changed_annotations = layout_inputs(
            5,
            helix_view::presentation_state::AnnotationSnapshot::new(helix_view::Revision::from(8)),
            120,
        );
        let changed_width = layout_inputs(
            5,
            helix_view::presentation_state::AnnotationSnapshot::new(helix_view::Revision::from(7)),
            121,
        );

        assert!(previous.can_reuse_seed_line_map(&same_text));
        assert!(!previous.can_reuse_seed_line_map(&changed_doc));
        assert!(!previous.can_reuse_seed_line_map(&changed_annotations));
        assert!(!previous.can_reuse_seed_line_map(&changed_width));
    }

    #[test]
    fn restore_focus_after_mouse_scroll_does_not_recentre_same_view() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let _guard = runtime.enter();
        let text = (0..200)
            .map(|idx| format!("line {idx}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (mut editor, view_id, doc_id) = test_editor_with_text(&text);

        {
            let doc = editor.document_mut(doc_id).expect("document");
            doc.set_view_offset(
                view_id,
                ViewPosition {
                    anchor: doc.text().line_to_char(50),
                    vertical_offset: 0,
                    horizontal_offset: 0,
                },
            );
        }

        let before = editor
            .document(doc_id)
            .expect("document")
            .view_offset(view_id);
        editor.with_temporary_focus(view_id, |_| {});
        let after = editor
            .document(doc_id)
            .expect("document")
            .view_offset(view_id);

        assert_eq!(after, before);
    }

    #[test]
    fn content_area_excludes_statusline_row() {
        let mut view = View::new(DocumentId::default(), Default::default());
        view.area = Rect::new(5, 7, 80, 10);

        assert_eq!(EditorView::content_area(&view), Rect::new(5, 7, 80, 9));
    }

    #[test]
    fn bufferline_renders_buffer_labels_with_native_padding() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let _guard = runtime.enter();
        let (mut editor, _view_id, doc_id) = test_editor_with_text("fn main() {}\n");
        let syn_loader = editor.syn_loader.load();

        {
            let doc = editor.document_mut(doc_id).expect("document");
            doc.set_path(Some(Path::new("main.rs")));
            let _ = doc.set_language_by_language_id("rust", &syn_loader);
        }

        let scratch_one = Document::from(
            Rope::from("# One\n"),
            None,
            editor.config.clone(),
            editor.syn_loader.clone(),
        )
        .with_persistent_scratch();
        editor.new_file_from_document(Action::VerticalSplit, scratch_one);

        let scratch_two = Document::from(
            Rope::from("# Two\n"),
            None,
            editor.config.clone(),
            editor.syn_loader.clone(),
        )
        .with_persistent_scratch();
        editor.new_file_from_document(Action::VerticalSplit, scratch_two);

        let mut editor_view = test_editor_view();
        let area = Rect::new(0, 0, 80, 1);
        let mut surface = CellSurface::empty(tui::ratatui::to_ratatui_rect(area));
        let config = editor.config();
        let model = BufferlineModel {
            theme: &editor.theme,
            separator: config.bufferline.separator.clone(),
            current_doc: editor.focused_document_id(),
            documents: editor
                .documents()
                .map(|doc| BufferlineDocument {
                    id: doc.id(),
                    label: editor.buffer_label(doc),
                    path: doc.path(),
                    language_name: doc.language_name(),
                    is_modified: doc.is_modified(),
                })
                .collect(),
        };
        editor_view.draw_bufferline_model(&model, area, &mut surface);

        let second_x = editor_view.bufferline_info.visible_buffers[1].columns.start;
        let third_x = editor_view.bufferline_info.visible_buffers[2].columns.start;
        let row: String = (0..area.width)
            .map(|x| surface[(area.x + x, area.y)].symbol())
            .collect();

        assert_eq!(
            surface[(second_x - 1, 0)].symbol(),
            "│",
            "row={row:?} ranges={:?}",
            editor_view.bufferline_info.visible_buffers
        );
        assert_eq!(
            surface[(second_x, 0)].symbol(),
            " ",
            "row={row:?} ranges={:?}",
            editor_view.bufferline_info.visible_buffers
        );
        assert_eq!(
            surface[(second_x + 1, 0)].symbol(),
            "[",
            "row={row:?} ranges={:?}",
            editor_view.bufferline_info.visible_buffers
        );
        assert_eq!(
            surface[(third_x - 1, 0)].symbol(),
            "│",
            "row={row:?} ranges={:?}",
            editor_view.bufferline_info.visible_buffers
        );
        assert_eq!(
            surface[(third_x, 0)].symbol(),
            " ",
            "row={row:?} ranges={:?}",
            editor_view.bufferline_info.visible_buffers
        );
        assert_eq!(
            surface[(third_x + 1, 0)].symbol(),
            "[",
            "row={row:?} ranges={:?}",
            editor_view.bufferline_info.visible_buffers
        );
    }

    #[test]
    #[ignore = "targeted local repro for full render_view on many giant lines"]
    fn render_view_many_giant_lines_repro() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let _guard = runtime.enter();

        for (lines, bytes_per_line) in [(20, 4_000), (50, 5_000), (100, 18_500), (2, 900_000)] {
            let event_log_path = std::env::temp_dir().join(format!(
                "helix-render-view-giant-lines-{}-{}-{}.log",
                std::process::id(),
                lines,
                bytes_per_line
            ));
            let _ = std::fs::remove_file(&event_log_path);
            let _run_guard =
                helix_view::bench::enter_bench_run(helix_view::bench::BenchRunContext {
                    seed: 0,
                    event_log_path: event_log_path.clone(),
                });

            let text = giant_multiline_fixture(lines, bytes_per_line);
            let (mut editor, view_id, doc_id) = test_editor_with_text(&text);
            editor.resize(Rect::new(0, 0, 160, 61));

            {
                let doc = editor.document_mut(doc_id).expect("document");
                doc.set_view_offset(
                    view_id,
                    ViewPosition {
                        anchor: doc.text().line_to_char(lines / 2),
                        vertical_offset: 0,
                        horizontal_offset: 0,
                    },
                );
            }

            let editor_view = test_editor_view();
            let area = Rect::new(0, 0, 160, 61);
            let mut surface = CellSurface::empty(tui::ratatui::to_ratatui_rect(area));
            let start = std::time::Instant::now();
            let doc = editor.document(doc_id).expect("document");
            let view = view!(editor, view_id);
            let config = editor.config();
            let vctx = ViewRenderContext {
                doc,
                view,
                viewport: area,
                is_focused: true,
                config: &config,
                config_gen: editor.config_gen,
                theme: &editor.theme,
                mode: editor.mode(),
                syntax_loader: &editor.syn_loader,
                cursor_cache: &editor.cursor_cache,
                gutter_context: GutterContext {
                    mode: editor.mode(),
                    line_number: config.line_number,
                    wrap_indicator: config
                        .soft_wrap
                        .wrap_indicator
                        .as_deref()
                        .map_or(Cow::Borrowed("↪"), Cow::Borrowed),
                    breakpoints: doc
                        .path()
                        .and_then(|path| editor.breakpoints.get(path))
                        .map(Vec::as_slice),
                    debug_execution: None,
                },
                debug_execution: None,
                cached_syntax: None,
                dirty_rows: None,
                seed_line_map: None,
            };
            let output = editor_view.render_view(&vctx, &mut surface);

            eprintln!(
                "render_view_many_giant_lines_repro: lines={} bytes_per_line={} elapsed_us={} mapped_lines={} bytes={}",
                lines,
                bytes_per_line,
                start.elapsed().as_micros(),
                output.line_map.lines.len(),
                doc.text().len_bytes(),
            );
            if let Ok(trace) = std::fs::read_to_string(&event_log_path) {
                eprintln!("{trace}");
            }
        }
    }
}
