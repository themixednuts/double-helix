// Each component declares its own size constraints and gets fitted based on its parent.
// Q: how does this work with popups?
// cursive does compositor.screen_mut().add_layer_at(pos::absolute(x, y), <component>)

/// Auto-wire widget trait accessors in a `Component` impl block.
///
/// Instead of manually implementing `as_focusable`, `is_focused`, and
/// `as_scrollable`, use this macro in your `impl Component for T` block:
///
/// ```ignore
/// impl Component for MyPanel {
///     component_traits!(focusable);           // if T: Focusable
///     component_traits!(scrollable);          // if T: Scrollable
///     component_traits!(focusable, scrollable); // both
///     // ... rest of Component methods
/// }
/// ```
#[macro_export]
macro_rules! component_traits {
    (focusable) => {
        fn is_focused(&self) -> bool {
            helix_view::traits::Focusable::is_focused(self)
        }
        fn as_focusable(&mut self) -> Option<&mut dyn helix_view::traits::Focusable> {
            Some(self)
        }
    };
    (scrollable) => {
        fn as_scrollable(&mut self) -> Option<&mut dyn helix_view::traits::Scrollable> {
            Some(self)
        }
    };
    ($first:ident, $($rest:ident),+) => {
        component_traits!($first);
        $(component_traits!($rest);)+
    };
}

use helix_core::Position;
use helix_view::graphics::{CursorKind, Rect};
use helix_view::input::{MouseButton, MouseEvent, MouseEventKind};
use helix_view::model::{PanelId, PanelSide, PanelSize};
use helix_view::bench::log_run_phase;
use crate::render::{CacheStore, PreparedRender, RenderOutput};

use tui::buffer::Buffer as Surface;

pub type Callback = Box<dyn FnOnce(&mut Compositor, &mut Context) + Send>;
pub type SyncCallback = Box<dyn FnOnce(&mut Compositor, &mut Context) + Send + Sync>;

// Cursive-inspired
pub enum EventResult {
    Ignored(Option<Callback>),
    Consumed(Option<Callback>),
}

use crate::job::Jobs;
use crate::ui::picker;
use helix_view::Editor;
use helix_view::keyboard::{KeyCode, KeyModifiers};
use log::warn;

use helix_plugin::PluginManager;

/// Layout computed from `Model.panels`. Describes how the editor area is
/// split to accommodate docked panels.
#[derive(Debug, Clone)]
pub(crate) struct PanelLayout {
    /// Area for the main editor content.
    pub editor_area: Rect,
    /// Panel areas keyed by `PanelId` — type-safe, no string matching.
    pub panel_areas: Vec<(PanelId, Rect)>,
}

/// Compute panel layout from `editor.model.panels`.
///
/// Panels reduce the editor area from their docked side. Multiple panels on
/// the same side stack (each takes from the remaining space). The order is
/// determined by iteration order of the SlotMap.
pub(crate) fn compute_panel_layout(area: Rect, editor: &Editor) -> PanelLayout {
    let mut editor_area = area;
    let mut panel_areas = Vec::new();

    for (panel_id, panel) in &editor.model.panels {
        let axis_total = match panel.side {
            PanelSide::Left | PanelSide::Right => area.width,
            PanelSide::Bottom => area.height,
        };
        let panel_size = match panel.size {
            PanelSize::Fixed(px) => px.map_or(0, |n| n.get()),
            PanelSize::Percent(pct) => (axis_total as u32 * pct as u32 / 100) as u16,
            PanelSize::Fill => axis_total,
            PanelSize::Constrained { min, max } => axis_total.clamp(min, max.get()),
        };

        match panel.side {
            PanelSide::Right => {
                if editor_area.width <= 60 {
                    continue; // too narrow to split
                }
                let w = panel_size.max(30).min(editor_area.width.saturating_sub(40));
                let panel_rect = Rect {
                    x: editor_area.x + editor_area.width - w,
                    y: editor_area.y,
                    width: w,
                    height: editor_area.height,
                };
                editor_area.width = editor_area.width.saturating_sub(w);
                panel_areas.push((panel_id, panel_rect));
            }
            PanelSide::Left => {
                if editor_area.width <= 60 {
                    continue;
                }
                let w = panel_size.max(20).min(editor_area.width.saturating_sub(40));
                let panel_rect = Rect {
                    x: editor_area.x,
                    y: editor_area.y,
                    width: w,
                    height: editor_area.height,
                };
                editor_area.x += w;
                editor_area.width = editor_area.width.saturating_sub(w);
                panel_areas.push((panel_id, panel_rect));
            }
            PanelSide::Bottom => {
                if editor_area.height <= 10 {
                    continue;
                }
                let h = panel_size.max(5).min(editor_area.height.saturating_sub(5));
                let panel_rect = Rect {
                    x: editor_area.x,
                    y: editor_area.y + editor_area.height - h,
                    width: editor_area.width,
                    height: h,
                };
                editor_area.height = editor_area.height.saturating_sub(h);
                panel_areas.push((panel_id, panel_rect));
            }
        }
    }

    warn!(
        "[layout] terminal_area=({},{} {}x{}) editor_area=({},{} {}x{}) panels={:?}",
        area.x, area.y, area.width, area.height,
        editor_area.x, editor_area.y, editor_area.width, editor_area.height,
        panel_areas.iter().map(|(id, r)| format!("{:?}=({},{} {}x{})", id, r.x, r.y, r.width, r.height)).collect::<Vec<_>>(),
    );

    PanelLayout {
        editor_area,
        panel_areas,
    }
}

/// Resolve the area for a component based on its layout role.
fn resolve_area(layer: &dyn Component, full_area: Rect, layout: &PanelLayout) -> Rect {
    match layer.layout_role() {
        LayoutRole::Fill => layout.editor_area,
        LayoutRole::Docked => layer
            .panel_id()
            .and_then(|pid| {
                layout
                    .panel_areas
                    .iter()
                    .find(|(id, _)| *id == pid)
                    .map(|(_, rect)| *rect)
            })
            .unwrap_or_default(),
        LayoutRole::Overlay => full_area,
    }
}

pub use helix_view::input::Event;

/// How a component participates in layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LayoutRole {
    /// Takes remaining space after docked components (e.g. EditorView).
    Fill,
    /// Docked to a side — area looked up by `panel_id()`.
    Docked,
    /// Floats over everything, gets full terminal area (e.g. picker, popup).
    #[default]
    Overlay,
}

pub struct Context<'a> {
    pub editor: &'a mut Editor,
    pub scroll: Option<usize>,
    pub jobs: &'a mut Jobs,
    pub plugin_manager: Option<std::sync::Arc<PluginManager>>,
}

impl Context<'_> {
    /// Waits on all pending jobs, and then tries to flush all pending write
    /// operations for all documents.
    pub fn block_try_flush_writes(&mut self) -> anyhow::Result<()> {
        tokio::task::block_in_place(|| helix_lsp::block_on(self.jobs.finish(self.editor, None)))?;
        tokio::task::block_in_place(|| helix_lsp::block_on(self.editor.flush_writes()))?;
        Ok(())
    }
}

/// Immutable render context shared by all components during the render phase.
/// Created once per frame after sync + pre-render mutations complete.
pub struct RenderContext<'a> {
    pub editor: &'a Editor,
    /// Scroll offset communicated from parent (e.g. Popup) to child during render.
    /// Uses `AtomicUsize` for Sync-safe interior mutability. `usize::MAX` = None.
    scroll: std::sync::atomic::AtomicUsize,
    pub plugin_manager: Option<std::sync::Arc<PluginManager>>,
}

const SCROLL_NONE: usize = usize::MAX;

impl RenderContext<'_> {
    pub fn scroll(&self) -> Option<usize> {
        let v = self.scroll.load(std::sync::atomic::Ordering::Relaxed);
        if v == SCROLL_NONE { None } else { Some(v) }
    }

    pub fn set_scroll(&self, value: Option<usize>) {
        self.scroll.store(
            value.unwrap_or(SCROLL_NONE),
            std::sync::atomic::Ordering::Relaxed,
        );
    }
}

/// Safety: `RenderContext` is only used during the render phase where `Editor`
/// is accessed immutably (no mutations). The `scroll` field uses `AtomicUsize`.
/// The remaining field (`plugin_manager`) is `Arc` which is already Sync.
/// `Editor` itself contains some non-Sync fields (e.g. `save_queue` with
/// `dyn Future + Send`) but these are never accessed during rendering — only
/// read-only config/document/view data is used. This `Sync` impl enables
/// sharing `&RenderContext` across rayon threads for parallel component render.
unsafe impl Sync for RenderContext<'_> {}

pub trait Component: Any + AnyComponent + Send {
    /// Process input events, return true if handled.
    fn handle_event(&mut self, _event: &Event, _ctx: &mut Context) -> EventResult {
        EventResult::Ignored(None)
    }
    // , args: ()

    /// Should redraw? Useful for saving redraw cycles if we know component didn't change.
    fn should_update(&self) -> bool {
        true
    }

    /// Sync component state to `editor.model`. Called by the compositor
    /// before layout computation and rendering, so Model is always up-to-date
    /// when layout and render functions read it.
    ///
    /// Components that participate in Model (panels, layers) should override
    /// this to push/update their state. The default does nothing.
    fn sync(&mut self, _editor: &mut Editor) {}

    /// Render the component onto the provided surface.
    fn render(&mut self, area: Rect, frame: &mut Surface, ctx: &RenderContext);

    /// Prepare an owned render artifact for later composition.
    ///
    /// The default renders eagerly into an offscreen buffer with no cache.
    /// Components that want caching construct a [`CacheTag`] and return
    /// [`PreparedRender::cached`] or [`PreparedRender::snapshot`].
    fn prepare_render(&mut self, area: Rect, ctx: &RenderContext) -> PreparedRender {
        let mut output = RenderOutput::new(area);
        self.render(area, &mut output.surface, ctx);
        PreparedRender::ready(output)
    }

    /// Get cursor position and cursor kind.
    fn cursor(&self, _area: Rect, _ctx: &Editor) -> (Option<Position>, CursorKind) {
        (None, CursorKind::Hidden)
    }

    /// May be used by the parent component to compute the child area.
    /// viewport is the maximum allowed area, and the child should stay within those bounds.
    ///
    /// The returned size might be larger than the viewport if the child is too big to fit.
    /// In this case the parent can use the values to calculate scroll.
    fn required_size(&mut self, _viewport: (u16, u16)) -> Option<(u16, u16)> {
        None
    }

    fn type_name(&self) -> &'static str {
        std::any::type_name::<Self>()
    }

    fn id(&self) -> Option<&'static str> {
        None
    }

    /// How this component participates in layout.
    fn layout_role(&self) -> LayoutRole {
        LayoutRole::Overlay
    }

    /// If this component is a docked panel, return its `PanelId`.
    /// Only meaningful when `layout_role() == LayoutRole::Docked`.
    fn panel_id(&self) -> Option<helix_view::model::PanelId> {
        None
    }

    /// Whether this component currently has focus (read-only).
    /// Used by the compositor for cursor routing without needing `&mut`.
    fn is_focused(&self) -> bool {
        false
    }

    /// Mutable focus access. Returns `Some` if this component supports
    /// focus management. The compositor calls this for mouse-click routing.
    fn as_focusable(&mut self) -> Option<&mut dyn helix_view::traits::Focusable> {
        None
    }

    /// Mutable scroll access. Returns `Some` if this component supports
    /// scrolling. The compositor calls this for mouse wheel routing.
    fn as_scrollable(&mut self) -> Option<&mut dyn helix_view::traits::Scrollable> {
        None
    }
}

pub struct Compositor {
    layers: Vec<Box<dyn Component>>,
    area: Rect,

    pub(crate) last_picker: Option<Box<dyn Component>>,
    pub(crate) full_redraw: bool,
    pending_timers: Vec<(crate::host::TimerId, std::time::Duration)>,
    /// Cached from the most recent render pass so mouse events can do hit-testing.
    last_layout: Option<PanelLayout>,
    render_cache: CacheStore,
}

impl Compositor {
    pub fn new(area: Rect) -> Self {
        Self {
            layers: Vec::new(),
            area,
            last_picker: None,
            full_redraw: false,
            pending_timers: Vec::new(),
            last_layout: None,
            render_cache: CacheStore::default(),
        }
    }

    pub fn size(&self) -> Rect {
        self.area
    }

    pub fn resize(&mut self, area: Rect) {
        self.area = area;
    }

    /// Add a layer to be rendered in front of all existing layers.
    pub fn push(&mut self, mut layer: Box<dyn Component>) {
        // immediately clear last_picker field to avoid excessive memory
        // consumption for picker with many items
        if layer.id() == Some(picker::ID) {
            self.last_picker = None;
        }
        let size = self.size();
        // trigger required_size on init
        layer.required_size((size.width, size.height));
        self.layers.push(layer);
    }

    /// Replace a component that has the given `id` with the new layer and if
    /// no component is found, push the layer normally.
    pub fn replace_or_push<T: Component>(&mut self, id: &'static str, layer: T) {
        if let Some(component) = self.find_id(id) {
            *component = layer;
        } else {
            self.push(Box::new(layer))
        }
    }

    pub fn pop(&mut self) -> Option<Box<dyn Component>> {
        self.layers.pop()
    }

    pub fn layer_count(&self) -> usize {
        self.layers.len()
    }

    pub fn remove(&mut self, id: &'static str) -> Option<Box<dyn Component>> {
        self.remove_by_id(id)
    }

    pub fn remove_by_id(&mut self, id: &str) -> Option<Box<dyn Component>> {
        let idx = self
            .layers
            .iter()
            .position(|layer| layer.id() == Some(id))?;
        Some(self.layers.remove(idx))
    }

    pub fn remove_type<T: 'static>(&mut self) {
        let type_name = std::any::type_name::<T>();
        self.layers
            .retain(|component| component.type_name() != type_name);
    }
    pub fn handle_event(&mut self, event: &Event, cx: &mut Context) -> bool {
        // Canonicalize key events: strip SHIFT from Char keys so that e.g.
        // <S-j> becomes 'J'. This is done once here so every component sees
        // the same canonical form — no per-component canonicalization needed.
        let event = &match event {
            Event::Key(key) => {
                let mut key = *key;
                if let KeyCode::Char(_) = key.code {
                    key.modifiers.remove(KeyModifiers::SHIFT);
                }
                Event::Key(key)
            }
            other => other.clone(),
        };

        // If it is a key event, a macro is being recorded, and a macro isn't being replayed,
        // push the key event to the recording.
        if let (Event::Key(key), Some((_, keys))) = (event, &mut cx.editor.macro_recording) {
            if cx.editor.macro_replaying.is_empty() {
                keys.push(*key);
            }
        }

        // Mouse-panel interaction: click to focus, scroll to scroll.
        if let Event::Mouse(mouse) = event {
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    self.handle_mouse_panel_focus(mouse.column, mouse.row);
                }
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                    if self.handle_mouse_panel_scroll(mouse) =>
                {
                    return true;
                }
                _ => {}
            }
        }

        let mut callbacks = Vec::new();
        let mut consumed = false;

        // propagate events through the layers until we either find a layer that consumes it or we
        // run out of layers (event bubbling), starting at the front layer and then moving to the
        // background.
        for layer in self.layers.iter_mut().rev() {
            let layer_start = std::time::Instant::now();
            let result = layer.handle_event(event, cx);
            log_run_phase("dispatch_layer", layer.type_name(), layer_start.elapsed(), || {
                format!(
                    "id={} event={} role={:?}",
                    layer.id().unwrap_or("-"),
                    match event {
                        Event::Key(_) => "key",
                        Event::Mouse(_) => "mouse",
                        Event::Resize(..) => "resize",
                        Event::IdleTimeout => "idle",
                        Event::Paste(_) => "paste",
                        Event::FocusGained => "focus_gained",
                        Event::FocusLost => "focus_lost",
                    },
                    layer.layout_role()
                )
            });
            match result {
                EventResult::Consumed(Some(callback)) => {
                    callbacks.push(callback);
                    consumed = true;
                    break;
                }
                EventResult::Consumed(None) => {
                    consumed = true;
                    break;
                }
                EventResult::Ignored(Some(callback)) => {
                    callbacks.push(callback);
                }
                EventResult::Ignored(None) => {}
            };
        }

        for callback in callbacks {
            callback(self, cx)
        }

        consumed
    }

    /// Focus the panel under the mouse click, or unfocus all panels if the
    /// click landed in the editor area.
    fn handle_mouse_panel_focus(&mut self, col: u16, row: u16) {
        let layout = match &self.last_layout {
            Some(l) => l,
            None => return,
        };

        // Which panel was clicked (if any)?
        let clicked_panel = layout
            .panel_areas
            .iter()
            .find(|(_, rect)| rect.contains(col, row));

        match clicked_panel {
            Some((panel_id, _)) => {
                let pid = *panel_id;
                warn!(
                    "[mouse_focus] click at ({},{}) → panel {:?}",
                    col, row, pid
                );
                for layer in &mut self.layers {
                    let is_target = layer.panel_id() == Some(pid);
                    if let Some(focusable) = layer.as_focusable() {
                        let was = focusable.is_focused();
                        focusable.set_focused(is_target);
                        warn!(
                            "[mouse_focus]   layer id={} panel_id={:?} was_focused={} now_focused={}",
                            layer.id().unwrap_or("-"),
                            layer.panel_id(),
                            was,
                            is_target,
                        );
                    }
                }
            }
            None => {
                warn!(
                    "[mouse_focus] click at ({},{}) → editor area (unfocus all)",
                    col, row
                );
                // Click in editor area — unfocus all panels.
                for layer in &mut self.layers {
                    if let Some(focusable) = layer.as_focusable() {
                        let was = focusable.is_focused();
                        focusable.set_focused(false);
                        warn!(
                            "[mouse_focus]   layer id={} panel_id={:?} was_focused={} now_focused=false",
                            layer.id().unwrap_or("-"),
                            layer.panel_id(),
                            was,
                        );
                    }
                }
            }
        }
    }

    /// Route a scroll event to the panel under the mouse cursor.
    /// Returns `true` if the scroll was handled by a panel.
    fn handle_mouse_panel_scroll(&mut self, mouse: &MouseEvent) -> bool {
        let layout = match &self.last_layout {
            Some(l) => l,
            None => return false,
        };

        let col = mouse.column;
        let row = mouse.row;

        // Find the panel under the cursor.
        let panel = layout
            .panel_areas
            .iter()
            .find(|(_, rect)| rect.contains(col, row));

        let pid = match panel {
            Some((id, _rect)) => *id,
            None => return false,
        };

        // Find the component for this panel and scroll it.
        for layer in &mut self.layers {
            if layer.panel_id() == Some(pid) {
                if let Some(scrollable) = layer.as_scrollable() {
                    let current = scrollable.scroll();
                    let max = scrollable.max_scroll();
                    let content_h = scrollable.content_height();
                    let viewport_h = scrollable.area().height;
                    match mouse.kind {
                        MouseEventKind::ScrollUp => {
                            let new = current.saturating_sub(3);
                            warn!("[panel_scroll] UP current={current} new={new} max={max} content_h={content_h} viewport_h={viewport_h}");
                            scrollable.scroll_to(new);
                        }
                        MouseEventKind::ScrollDown => {
                            let new = (current + 3).min(max);
                            warn!("[panel_scroll] DOWN current={current} new={new} max={max} content_h={content_h} viewport_h={viewport_h}");
                            scrollable.scroll_to(new);
                        }
                        _ => {}
                    }
                    return true;
                }
            }
        }

        false
    }

    pub fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        // Phase 1: Sync — all components push state to Model before layout.
        // This is the ONLY phase with `&mut Editor` — mutations happen here.
        for layer in &mut self.layers {
            let sync_start = std::time::Instant::now();
            layer.sync(cx.editor);
            log_run_phase("compositor_sync", layer.type_name(), sync_start.elapsed(), || {
                format!(
                    "id={} role={:?}",
                    layer.id().unwrap_or("-"),
                    layer.layout_role()
                )
            });
        }

        // Pre-render mutations (need &mut Editor, done before freeze).
        cx.editor.cleanup_notifications();

        // Phase 2: Layout — read Model.panels for data-driven area splits.
        let layout_start = std::time::Instant::now();
        let has_prompt = self.has_component("helix_term::ui::prompt::Prompt");
        let layout = compute_panel_layout(area, cx.editor);
        self.last_layout = Some(layout.clone());
        log_run_phase("compositor_layout", "compute", layout_start.elapsed(), || {
            format!(
                "layers={} panel_areas={} editor_area={}x{}+{},{} has_prompt={}",
                self.layers.len(),
                layout.panel_areas.len(),
                layout.editor_area.width,
                layout.editor_area.height,
                layout.editor_area.x,
                layout.editor_area.y,
                has_prompt,
            )
        });

        // Pre-render mutation: resize editor area (was in EditorView::render,
        // moved here so render phase can be fully immutable).
        // Use layout.editor_area (panel-adjusted) so the editor tree doesn't
        // extend into docked panel areas.
        {
            let config = cx.editor.config();
            use helix_view::editor::{BufferLineRenderMode, CmdlineStyle};
            let use_bufferline = match config.bufferline.render_mode {
                BufferLineRenderMode::Always => true,
                BufferLineRenderMode::Multiple if cx.editor.documents.len() > 1 => true,
                _ => false,
            };
            let mut editor_area = if config.cmdline.style == CmdlineStyle::Popup
                && config.cmdline.use_full_height
            {
                layout.editor_area
            } else {
                layout.editor_area.clip_bottom(1)
            };
            if use_bufferline {
                editor_area = editor_area.clip_top(1);
            }
            warn!(
                "[resize] editor.resize area=({},{} {}x{}) use_bufferline={} cmdline_popup={}",
                editor_area.x, editor_area.y, editor_area.width, editor_area.height,
                use_bufferline,
                config.cmdline.style == CmdlineStyle::Popup && config.cmdline.use_full_height,
            );
            cx.editor.resize(editor_area);
        }

        // Freeze: create immutable render context. All render phases below
        // use &RenderContext — no &mut Editor access.
        let render_ctx = RenderContext {
            editor: cx.editor,
            scroll: std::sync::atomic::AtomicUsize::new(
                cx.scroll.unwrap_or(SCROLL_NONE),
            ),
            plugin_manager: cx.plugin_manager.clone(),
        };

        // Set prompt_active on EditorView before render.
        for layer in &mut self.layers {
            if let Some(editor_view) = layer.as_any_mut().downcast_mut::<crate::ui::EditorView>() {
                editor_view.prompt_active = has_prompt;
            }
        }

        // Phase 3: Render — Fill and Docked layers.
        // Split layers at overlay boundary, then prepare base layers in parallel via rayon.
        use rayon::prelude::*;

        // Find where overlays begin.
        let overlay_start = self.layers.iter()
            .position(|l| l.layout_role() == LayoutRole::Overlay)
            .unwrap_or(self.layers.len());

        // Parallel prepare: split layers, compute areas, then par_iter_mut on base.
        let base_batch: Vec<PreparedRender> = {
            let base_layers = &mut self.layers[..overlay_start];

            let base_areas: Vec<Rect> = base_layers.iter()
                .map(|l| resolve_area(l.as_ref(), area, &layout))
                .collect();

            base_layers.par_iter_mut()
                .zip(base_areas.par_iter())
                .map(|(layer, &layer_area)| layer.prepare_render(layer_area, &render_ctx))
                .collect()
        };

        {
            let base_start = std::time::Instant::now();
            let count = base_batch.len();
            self.render_cache.compose_batch(base_batch, surface);
            log_run_phase("compositor_layer", "base_batch", base_start.elapsed(), || {
                format!("count={count} phase=base_parallel")
            });
        }

        // Phase 4: Info popup — on top of panels but below overlays.
        if render_ctx.editor.config().auto_info {
            if let Some(info) = render_ctx.editor.autoinfo.as_ref() {
                let info_start = std::time::Instant::now();
                let mut info_copy = info.clone();
                info_copy.render(area, surface, &render_ctx);
                log_run_phase("compositor_layer", "autoinfo", info_start.elapsed(), || {
                    format!("id=autoinfo role=overlay area={}x{}+{},{}", area.width, area.height, area.x, area.y)
                });
            }
        }

        // Phase 5: Overlay layers (pickers, popups, prompts) on top of everything.
        // Overlays render directly to the main surface (not through prepare_render/blit)
        // because they only paint a small region — blitting a full-area surface would
        // overwrite the editor content underneath with empty cells.
        {
            let overlay_start_time = std::time::Instant::now();
            let overlay_layers = &mut self.layers[overlay_start..];
            let count = overlay_layers.len();
            for layer in overlay_layers.iter_mut() {
                let layer_area = resolve_area(layer.as_ref(), area, &layout);
                layer.render(layer_area, surface, &render_ctx);
            }
            log_run_phase("compositor_layer", "overlay_batch", overlay_start_time.elapsed(), || {
                format!("count={count} phase=overlay_direct")
            });
        }
    }

    pub fn cursor(&self, area: Rect, editor: &Editor) -> (Option<Position>, CursorKind) {
        let layout = compute_panel_layout(area, editor);

        let (cursor_pos, cursor_kind) = {
            let mut result = (None, CursorKind::Hidden);
            for layer in self.layers.iter().rev() {
                let layer_area = resolve_area(layer.as_ref(), area, &layout);
                if let (Some(pos), kind) = layer.cursor(layer_area, editor) {
                    result = (Some(pos), kind);
                    break;
                }
            }
            result
        };

        // Hide cursor if it falls inside the info popup area
        if let (Some(pos), _) = (cursor_pos, cursor_kind) {
            if editor.config().auto_info {
                if let Some(ref info) = editor.autoinfo {
                    let info_area = info.screen_area(area);
                    if info_area.contains(pos.col as u16, pos.row as u16) {
                        return (None, CursorKind::Hidden);
                    }
                }
            }
        }

        (cursor_pos, cursor_kind)
    }

    pub fn has_component(&self, type_name: &str) -> bool {
        self.layers
            .iter()
            .any(|component| component.type_name() == type_name)
    }

    pub fn find<T: 'static>(&mut self) -> Option<&mut T> {
        let type_name = std::any::type_name::<T>();
        self.layers
            .iter_mut()
            .find(|component| component.type_name() == type_name)
            .and_then(|component| component.as_any_mut().downcast_mut())
    }

    pub fn find_id<T: 'static>(&mut self, id: &'static str) -> Option<&mut T> {
        self.layers
            .iter_mut()
            .find(|component| component.id() == Some(id))
            .and_then(|component| component.as_any_mut().downcast_mut())
    }

    pub fn need_full_redraw(&mut self) {
        self.full_redraw = true;
    }

    /// Drain pending timer requests (for the event loop to schedule).
    pub fn take_pending_timers(&mut self) -> Vec<(crate::host::TimerId, std::time::Duration)> {
        std::mem::take(&mut self.pending_timers)
    }
}

impl crate::host::UiHost for Compositor {
    fn invalidate(&mut self, area: crate::host::Invalidation) {
        match area {
            crate::host::Invalidation::Full => self.full_redraw = true,
            crate::host::Invalidation::Rect(_) => {
                // Terminal backend doesn't support partial invalidation —
                // the tui Buffer handles cell-level diffing automatically.
                // Just mark for redraw.
                self.full_redraw = true;
            }
        }
    }

    fn request_timer(&mut self, id: crate::host::TimerId, after: std::time::Duration) {
        self.pending_timers.push((id, after));
    }
}

// View casting, taken straight from Cursive

use std::any::Any;

/// A view that can be downcasted to its concrete type.
///
/// This trait is automatically implemented for any `T: Component`.
pub trait AnyComponent {
    /// Downcast self to a `Any`.
    fn as_any(&self) -> &dyn Any;

    /// Downcast self to a mutable `Any`.
    fn as_any_mut(&mut self) -> &mut dyn Any;

    /// Returns a boxed any from a boxed self.
    ///
    /// Can be used before `Box::downcast()`.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use helix_term::{ui::Text, compositor::Component};
    /// let boxed: Box<dyn Component> = Box::new(Text::new("text".to_string()));
    /// let text: Box<Text> = boxed.as_boxed_any().downcast().unwrap();
    /// ```
    fn as_boxed_any(self: Box<Self>) -> Box<dyn Any>;
}

impl<T: Component> AnyComponent for T {
    /// Downcast self to a `Any`.
    fn as_any(&self) -> &dyn Any {
        self
    }

    /// Downcast self to a mutable `Any`.
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn as_boxed_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

impl dyn AnyComponent {
    /// Attempts to downcast `self` to a concrete type.
    pub fn downcast_ref<T: Any>(&self) -> Option<&T> {
        self.as_any().downcast_ref()
    }

    /// Attempts to downcast `self` to a concrete type.
    pub fn downcast_mut<T: Any>(&mut self) -> Option<&mut T> {
        self.as_any_mut().downcast_mut()
    }

    /// Attempts to downcast `Box<Self>` to a concrete type.
    pub fn downcast<T: Any>(self: Box<Self>) -> Result<Box<T>, Box<Self>> {
        // Do the check here + unwrap, so the error
        // value is `Self` and not `dyn Any`.
        if self.as_any().is::<T>() {
            Ok(self.as_boxed_any().downcast().unwrap())
        } else {
            Err(self)
        }
    }

    /// Checks if this view is of type `T`.
    pub fn is<T: Any>(&mut self) -> bool {
        self.as_any().is::<T>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_swap::ArcSwap;
    use helix_view::model::AcpModel;
    use std::sync::Arc;

    fn test_editor(width: u16, height: u16) -> Editor {
        let theme_loader = helix_view::theme::Loader::new(helix_loader::runtime_dirs());
        let syn_loader = helix_core::config::default_lang_loader();
        let config = helix_view::editor::Config::default();
        let config = Arc::new(ArcSwap::from_pointee(config));
        let handlers = helix_view::handlers::Handlers::dummy();
        Editor::new(
            Rect::new(0, 0, width, height),
            Arc::new(theme_loader),
            Arc::new(ArcSwap::from_pointee(syn_loader)),
            Arc::new(arc_swap::access::Map::new(
                config,
                |c: &helix_view::editor::Config| c,
            )),
            handlers,
        )
    }

    #[tokio::test]
    async fn panel_layout_right_percent35_splits_correctly() {
        let mut editor = test_editor(120, 40);
        editor.model.insert_panel(
            "ACP",
            Box::new(AcpModel::default()),
            PanelSide::Right,
            PanelSize::Percent(35),
        );
        let layout = compute_panel_layout(Rect::new(0, 0, 120, 40), &editor);

        // 35% of 120 = 42, clamped to min 30, max (120-40)=80 → 42
        assert_eq!(layout.panel_areas.len(), 1);
        let (_, panel_rect) = &layout.panel_areas[0];
        assert_eq!(panel_rect.width, 42);
        assert_eq!(layout.editor_area.width, 78); // 120 - 42
        assert_eq!(layout.editor_area.x, 0);
        assert_eq!(panel_rect.x, 78);
    }

    #[tokio::test]
    async fn panel_layout_right_hidden_when_narrow() {
        let mut editor = test_editor(50, 24);
        editor.model.insert_panel(
            "ACP",
            Box::new(AcpModel::default()),
            PanelSide::Right,
            PanelSize::Percent(35),
        );
        let layout = compute_panel_layout(Rect::new(0, 0, 50, 24), &editor);

        // Terminal too narrow (≤60) — panel should be skipped
        assert!(layout.panel_areas.is_empty());
        assert_eq!(layout.editor_area.width, 50);
    }

    #[tokio::test]
    async fn panel_layout_uses_panel_id_not_string() {
        let mut editor = test_editor(120, 40);
        let panel_id = editor.model.insert_panel(
            "ACP",
            Box::new(AcpModel::default()),
            PanelSide::Right,
            PanelSize::Percent(35),
        );
        let layout = compute_panel_layout(Rect::new(0, 0, 120, 40), &editor);

        // Layout is keyed by PanelId — impossible to mismatch with component.
        let (id, rect) = &layout.panel_areas[0];
        assert_eq!(*id, panel_id);
        assert!(rect.width > 0);
    }

    #[tokio::test]
    async fn panel_layout_editor_area_never_zero() {
        let mut editor = test_editor(80, 24);
        editor.model.insert_panel(
            "ACP",
            Box::new(AcpModel::default()),
            PanelSide::Right,
            PanelSize::Percent(90),
        );
        let layout = compute_panel_layout(Rect::new(0, 0, 80, 24), &editor);

        // Even with 90%, editor should keep at least 40 cols
        assert!(
            layout.editor_area.width >= 40,
            "editor width: {}",
            layout.editor_area.width
        );
    }
}
