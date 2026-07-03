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

use crate::render::{CacheStore, CellSurface, PreparedRender, RenderOutput};
use helix_core::Position;
use helix_runtime::FrameHandle;
use helix_view::bench::log_run_phase;
use helix_view::graphics::{CursorKind, Rect};
use helix_view::input::{MouseButton, MouseEvent, MouseEventKind};
use helix_view::model::{PanelId, PanelSide, PanelSize};
use helix_view::Editor;
use std::sync::Arc;

mod render_frame;
pub use render_frame::{RenderContext, RenderSnapshot};

/// Typed requests emitted by components after handling an event.
pub enum PostAction {
    PopLayer {
        model_layer: Option<helix_view::model::LayerId>,
        remember_picker: bool,
    },
    RemoveById(&'static str),
    PushLayer(Box<dyn Component>),
    ReplaceOrPushLayer {
        id: &'static str,
        layer: Box<dyn Component>,
    },
    UpdateCompletionFilter(Option<char>),
    ClearCompletion,
    ShowCommandPalette {
        register: Option<char>,
        count: Option<std::num::NonZeroUsize>,
    },
    RestoreLastPicker,
    ReplayKeys {
        keys: Vec<helix_view::input::KeyEvent>,
        count: usize,
        pop_macro_replaying: bool,
    },
    Batch(Vec<PostAction>),
}

impl std::fmt::Debug for PostAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PopLayer {
                model_layer,
                remember_picker,
            } => f
                .debug_struct("PopLayer")
                .field("model_layer", model_layer)
                .field("remember_picker", remember_picker)
                .finish(),
            Self::RemoveById(id) => f.debug_tuple("RemoveById").field(id).finish(),
            Self::PushLayer(layer) => f
                .debug_struct("PushLayer")
                .field("id", &layer.id())
                .field("type", &layer.type_name())
                .finish(),
            Self::ReplaceOrPushLayer { id, layer } => f
                .debug_struct("ReplaceOrPushLayer")
                .field("id", id)
                .field("layer_id", &layer.id())
                .field("layer_type", &layer.type_name())
                .finish(),
            Self::UpdateCompletionFilter(c) => {
                f.debug_tuple("UpdateCompletionFilter").field(c).finish()
            }
            Self::ClearCompletion => f.write_str("ClearCompletion"),
            Self::ShowCommandPalette { register, count } => f
                .debug_struct("ShowCommandPalette")
                .field("register", register)
                .field("count", count)
                .finish(),
            Self::RestoreLastPicker => f.write_str("RestoreLastPicker"),
            Self::ReplayKeys {
                keys,
                count,
                pop_macro_replaying,
            } => f
                .debug_struct("ReplayKeys")
                .field("keys", keys)
                .field("count", count)
                .field("pop_macro_replaying", pop_macro_replaying)
                .finish(),
            Self::Batch(actions) => f.debug_tuple("Batch").field(actions).finish(),
        }
    }
}

// Cursive-inspired
pub enum EventResult {
    Ignored(Option<PostAction>),
    Consumed(Option<PostAction>),
}

use crate::runtime::ExitTaskSet;
use crate::ui::picker;

use helix_plugin::PluginManager;

/// Layout computed from `Model.panels`. Describes how the editor area is
/// split to accommodate docked panels.
#[derive(Debug, Clone)]
pub(crate) struct PanelLayout {
    /// Area for the main editor content.
    pub editor_area: Rect,
    /// Panel areas keyed by `PanelId` — type-safe, no string matching.
    pub panel_areas: Vec<(PanelId, Rect)>,
    /// Bottom row reserved for the global status line (errors, info, the
    /// active cmdline prompt). Spans the full terminal width so messages
    /// remain visible no matter which panel has focus. Zero-height when
    /// the cmdline is configured to take the full editor height.
    pub global_status_row: Rect,
}

/// Compute panel layout from `editor.model.panels`.
///
/// Panels reduce the editor area from their docked side. Multiple panels on
/// the same side stack (each takes from the remaining space). The order is
/// determined by iteration order of the SlotMap.
pub(crate) fn compute_panel_layout(area: Rect, editor: &Editor) -> PanelLayout {
    // Reserve the very bottom row of the terminal for the global status
    // line (cmdline / status_msg / errors). The editor lives above this row.
    // Side panels underlap it by one row because their own final internal row
    // is transient/error chrome; the global row paints over that row, leaving
    // the panel footer on the same baseline as the editor statusline.
    // The Popup-full-height cmdline opts out — there's no reserved row.
    let config = editor.config();
    let reserve_global = !matches!(
        (config.cmdline.style, config.cmdline.use_full_height),
        (helix_view::editor::CmdlineStyle::Popup, true)
    );
    drop(config);

    let (chrome_area, global_status_row) = if reserve_global && area.height > 1 {
        (
            area.clip_bottom(1),
            Rect {
                x: area.x,
                y: area.bottom().saturating_sub(1),
                width: area.width,
                height: 1,
            },
        )
    } else {
        (
            area,
            Rect {
                x: area.x,
                y: area.bottom(),
                width: area.width,
                height: 0,
            },
        )
    };

    let mut editor_area = chrome_area;
    let mut panel_areas = Vec::new();

    for (panel_id, panel) in &editor.model.panels {
        if !panel.visible {
            continue;
        }
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
                // Both the panel minimum (30) and the editor reserve (40) must fit.
                if editor_area.width < 70 {
                    continue; // too narrow to split
                }
                let w = panel_size.max(30).min(editor_area.width.saturating_sub(40));
                let panel_rect = Rect {
                    x: editor_area.x + editor_area.width - w,
                    y: area.y,
                    width: w,
                    height: area.height,
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
                    y: area.y,
                    width: w,
                    height: area.height,
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

    log::debug!(
        "[layout] terminal_area=({},{} {}x{}) editor_area=({},{} {}x{}) panels={:?}",
        area.x,
        area.y,
        area.width,
        area.height,
        editor_area.x,
        editor_area.y,
        editor_area.width,
        editor_area.height,
        panel_areas
            .iter()
            .map(|(id, r)| format!("{:?}=({},{} {}x{})", id, r.x, r.y, r.width, r.height))
            .collect::<Vec<_>>(),
    );

    PanelLayout {
        editor_area,
        panel_areas,
        global_status_row,
    }
}

/// Render the global status row at the bottom of the terminal.
///
/// Shows the editor's `status_msg` — set by `editor.set_status()`,
/// `editor.set_error()`, etc. — full terminal width, regardless of which
/// panel has focus. Empty (background fill) when there's no message, so the
/// row's baseline is always present and visible.
fn render_global_status_row(
    area: Rect,
    surface: &mut crate::render::CellSurface,
    ctx: &RenderContext,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let theme = ctx.theme();
    let bg_style = theme.get("ui.background");
    surface.set_style(
        tui::ratatui::to_ratatui_rect(area),
        tui::ratatui::to_ratatui_style(bg_style),
    );

    let Some((msg, severity)) = ctx.status_msg() else {
        return;
    };
    use helix_view::editor::Severity;
    let style = match severity {
        Severity::Error => theme.get("error"),
        Severity::Warning => theme.get("warning"),
        Severity::Info | Severity::Hint => theme
            .try_get("ui.text.inactive")
            .or_else(|| theme.try_get("comment"))
            .unwrap_or_else(|| theme.get("ui.text")),
    };
    surface.set_stringn(
        area.x.saturating_add(1),
        area.y,
        msg,
        area.width.saturating_sub(1) as usize,
        tui::ratatui::to_ratatui_style(bg_style.patch(style)),
    );
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
    /// Exit-bound task sink for compositor-owned flows that must finish typed task work.
    pub exit_tasks: &'a mut ExitTaskSet,
    pub exit_task_work: helix_runtime::Work,
    pub notifier: crate::handlers::local::Notifier,
    /// Typed ingress for async work that needs to return to the application loop.
    pub ingress: crate::runtime::RuntimeIngress,
    pub redraw: FrameHandle,
    pub idle_reset: crate::runtime::IdleResetHandle,
    pub plugin_manager: Option<Arc<PluginManager>>,
}

impl<'a> Context<'a> {
    pub fn new(
        editor: &'a mut Editor,
        exit_tasks: &'a mut ExitTaskSet,
        exit_task_work: helix_runtime::Work,
        notifier: crate::handlers::local::Notifier,
        ingress: crate::runtime::RuntimeIngress,
        idle_reset: crate::runtime::IdleResetHandle,
        plugin_manager: Option<Arc<PluginManager>>,
    ) -> Self {
        let redraw = editor.redraw_handle();
        Self {
            editor,
            scroll: None,
            exit_tasks,
            exit_task_work,
            notifier,
            ingress,
            redraw,
            idle_reset,
            plugin_manager,
        }
    }

    pub fn spawn_ui(
        &mut self,
        future: impl std::future::Future<Output = anyhow::Result<crate::runtime::UiCommand>>
            + Send
            + 'static,
    ) {
        crate::runtime::ingress::spawn_ui_command_with_future(
            self.editor.work(),
            future,
            self.ingress.clone(),
        );
    }

    pub fn spawn_task_event(
        &mut self,
        future: impl std::future::Future<Output = anyhow::Result<crate::runtime::RuntimeTaskEvent>>
            + Send
            + 'static,
    ) {
        crate::runtime::ingress::spawn_task_event_with_future(
            self.editor.work(),
            future,
            self.ingress.clone(),
        );
    }

    pub fn reset_idle_timer(&self) {
        self.idle_reset.request_reset();
    }

    pub fn exit_task_event(
        &mut self,
        future: impl std::future::Future<Output = anyhow::Result<crate::runtime::RuntimeTaskEvent>>
            + Send
            + 'static,
    ) {
        crate::runtime::schedule_exit_task(self.exit_tasks, &self.exit_task_work, future);
    }

    /// Waits on all pending async UI work, then tries to flush all pending write
    /// operations for all documents.
    pub fn block_try_flush_writes(&mut self) -> anyhow::Result<()> {
        crate::runtime::drain_exit_tasks_blocking(
            self.editor,
            self.exit_tasks,
            self.ingress.clone(),
            self.plugin_manager
                .clone()
                .expect("plugin manager must be available when flushing exit tasks"),
        )?;
        tokio::task::block_in_place(|| helix_lsp::block_on(self.editor.flush_writes()))?;
        Ok(())
    }
}

pub trait Component: Any + Send {
    /// Process input events, return true if handled.
    fn handle_event(&mut self, _event: &Event, _ctx: &mut Context) -> EventResult {
        EventResult::Ignored(None)
    }
    // , args: ()

    /// Sync component state to `editor.model`. Called by the compositor
    /// before layout computation and rendering, so Model is always up-to-date
    /// when layout and render functions read it.
    ///
    /// Components that participate in Model (panels, layers) should override
    /// this to push/update their state. The default does nothing.
    fn sync(&mut self, _editor: &mut Editor) {}

    /// Render the component onto a terminal-style cell surface.
    fn render(&mut self, area: Rect, frame: &mut CellSurface, ctx: &RenderContext);

    /// Prepare a native Ratatui render artifact for later composition.
    fn prepare_render(&mut self, area: Rect, ctx: &RenderContext) -> PreparedRender {
        let mut output = RenderOutput::new(area);
        self.render(area, output.surface_mut(), ctx);
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

    fn id(&self) -> Option<&str> {
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

    fn as_picker_component(&mut self) -> Option<&mut dyn PickerComponent> {
        None
    }
}

pub trait PickerComponent {
    fn instance_id(&self) -> picker::PickerInstanceId;

    fn request_preview_highlight(&mut self, editor: &mut Editor, path: std::path::PathBuf);

    fn apply_preview(
        &mut self,
        editor: &mut Editor,
        path: std::path::PathBuf,
        preview: picker::CachedPreview,
    );

    fn apply_preview_syntax(
        &mut self,
        editor: &mut Editor,
        path: std::path::PathBuf,
        syntax: helix_core::Syntax,
    );

    fn run_dynamic_query(&mut self, editor: &mut Editor, query: std::sync::Arc<str>);
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
            Event::Key(key) => Event::Key(key.canonicalize()),
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
                    if self.handle_autoinfo_scroll(mouse, cx) =>
                {
                    return true;
                }
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                    if self.handle_mouse_panel_scroll(mouse) =>
                {
                    return true;
                }
                _ => {}
            }
        }

        let mut post_actions = Vec::new();
        let mut consumed = false;

        // propagate events through the layers until we either find a layer that consumes it or we
        // run out of layers (event bubbling), starting at the front layer and then moving to the
        // background.
        for layer in self.layers.iter_mut().rev() {
            let layer_start = std::time::Instant::now();
            let result = layer.handle_event(event, cx);
            log_run_phase(
                "dispatch_layer",
                layer.type_name(),
                layer_start.elapsed(),
                || {
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
                },
            );
            if let Event::Key(key) = event {
                let result_label = match &result {
                    EventResult::Consumed(Some(_)) => "consumed+callback",
                    EventResult::Consumed(None) => "consumed",
                    EventResult::Ignored(Some(_)) => "ignored+callback",
                    EventResult::Ignored(None) => "ignored",
                };
                log::debug!(
                    "[assistant_dispatch] key={:?} mods={:?} layer_id={} type={} focused={} result={}",
                    key.code,
                    key.modifiers,
                    layer.id().unwrap_or("-"),
                    layer.type_name(),
                    layer.is_focused(),
                    result_label
                );
            } else if let Event::Mouse(mouse) = event {
                let result_label = match &result {
                    EventResult::Consumed(Some(_)) => "consumed+callback",
                    EventResult::Consumed(None) => "consumed",
                    EventResult::Ignored(Some(_)) => "ignored+callback",
                    EventResult::Ignored(None) => "ignored",
                };
                log::debug!(
                    "[assistant_dispatch] mouse={:?} col={} row={} layer_id={} type={} focused={} result={}",
                    mouse.kind,
                    mouse.column,
                    mouse.row,
                    layer.id().unwrap_or("-"),
                    layer.type_name(),
                    layer.is_focused(),
                    result_label
                );
            }
            match result {
                EventResult::Consumed(Some(action)) => {
                    post_actions.push(action);
                    consumed = true;
                    break;
                }
                EventResult::Consumed(None) => {
                    consumed = true;
                    break;
                }
                EventResult::Ignored(Some(action)) => {
                    post_actions.push(action);
                }
                EventResult::Ignored(None) => {}
            };
        }

        for action in post_actions {
            apply_post_action(self, cx, action);
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
                log::debug!(
                    "[mouse_focus] click at ({},{}) -> panel {:?}",
                    col,
                    row,
                    pid
                );
                for layer in &mut self.layers {
                    let is_target = layer.panel_id() == Some(pid);
                    if let Some(focusable) = layer.as_focusable() {
                        let was = focusable.is_focused();
                        focusable.set_focused(is_target);
                        log::debug!(
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
                log::debug!(
                    "[mouse_focus] click at ({},{}) → editor area (unfocus all)",
                    col,
                    row
                );
                // Click in editor area — unfocus all panels.
                for layer in &mut self.layers {
                    if let Some(focusable) = layer.as_focusable() {
                        let was = focusable.is_focused();
                        focusable.set_focused(false);
                        log::debug!(
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
                            log::debug!("[panel_scroll] UP current={current} new={new} max={max} content_h={content_h} viewport_h={viewport_h}");
                            scrollable.scroll_to(new);
                        }
                        MouseEventKind::ScrollDown => {
                            let new = (current + 3).min(max);
                            log::debug!("[panel_scroll] DOWN current={current} new={new} max={max} content_h={content_h} viewport_h={viewport_h}");
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

    fn handle_autoinfo_scroll(&mut self, mouse: &MouseEvent, cx: &mut Context) -> bool {
        if !cx.editor.config().auto_info {
            return false;
        }

        let Some(info) = cx.editor.autoinfo.as_mut() else {
            return false;
        };

        let area = info.screen_area(self.area);
        if !area.contains(mouse.column, mouse.row) {
            return false;
        }

        let visible_body_height = crate::ui::design::info_popup_body_height(area);
        match mouse.kind {
            MouseEventKind::ScrollUp => info.scroll_by(-3, visible_body_height),
            MouseEventKind::ScrollDown => info.scroll_by(3, visible_body_height),
            _ => {}
        }
        true
    }

    pub fn render(&mut self, area: Rect, surface: &mut CellSurface, cx: &mut Context) {
        self.render_to_cells(area, surface, cx);
    }

    pub fn render_frame(&mut self, area: Rect, cx: &mut Context) -> RenderOutput {
        let mut output = RenderOutput::new(area);
        self.render(area, output.surface_mut(), cx);
        output
    }

    fn render_to_cells(&mut self, area: Rect, surface: &mut CellSurface, cx: &mut Context) {
        // Phase 1: Sync — all components push state to Model before layout.
        // This is the ONLY phase with `&mut Editor` — mutations happen here.
        for layer in &mut self.layers {
            let sync_start = std::time::Instant::now();
            layer.sync(cx.editor);
            log_run_phase(
                "compositor_sync",
                layer.type_name(),
                sync_start.elapsed(),
                || {
                    format!(
                        "id={} role={:?}",
                        layer.id().unwrap_or("-"),
                        layer.layout_role()
                    )
                },
            );
        }

        // Pre-render mutations (need &mut Editor, done before freeze).
        cx.editor.cleanup_notifications();

        // Phase 2: Layout — read Model.panels for data-driven area splits.
        let layout_start = std::time::Instant::now();
        let has_prompt = self.has_component("helix_term::ui::prompt::Prompt");
        let layout = compute_panel_layout(area, cx.editor);
        self.last_layout = Some(layout.clone());
        log_run_phase(
            "compositor_layout",
            "compute",
            layout_start.elapsed(),
            || {
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
            },
        );

        // Pre-render mutation: resize editor area (was in EditorView::render,
        // moved here so render phase can be fully immutable).
        // Use layout.editor_area (panel-adjusted) so the editor tree doesn't
        // extend into docked panel areas.
        {
            let config = cx.editor.config();
            use helix_view::editor::BufferLineRenderMode;
            let use_bufferline = match config.bufferline.render_mode {
                BufferLineRenderMode::Always => true,
                BufferLineRenderMode::Multiple if cx.editor.has_multiple_documents() => true,
                _ => false,
            };
            // `layout.editor_area` is already clipped by `compute_panel_layout`
            // so the global status row sits beneath it. Don't double-clip
            // here — just trim the bufferline row off the top if enabled.
            let mut editor_area = layout.editor_area;
            if use_bufferline {
                editor_area = editor_area.clip_top(1);
            }
            log::debug!(
                "[resize] editor.resize area=({},{} {}x{}) use_bufferline={} cmdline_popup={}",
                editor_area.x,
                editor_area.y,
                editor_area.width,
                editor_area.height,
                use_bufferline,
                config.cmdline.style == helix_view::editor::CmdlineStyle::Popup
                    && config.cmdline.use_full_height,
            );
            cx.editor.resize(editor_area);
        }

        // Freeze: create immutable render context. All render phases below
        // use &RenderContext — no &mut Editor access.
        let render_ctx = RenderContext::with_scroll(
            cx.editor,
            cx.scroll,
            cx.ingress.clone(),
            cx.redraw.clone(),
            cx.plugin_manager.clone(),
        );

        // Set prompt_active on EditorView before render.
        for layer in &mut self.layers {
            if let Some(editor_view) = layer.downcast_mut::<crate::ui::EditorView>() {
                editor_view.prompt_active = has_prompt;
            }
        }

        // Phase 3: Render — Fill and Docked layers.
        // Find where overlays begin.
        let overlay_start = self
            .layers
            .iter()
            .position(|l| l.layout_role() == LayoutRole::Overlay)
            .unwrap_or(self.layers.len());

        {
            let base_start = std::time::Instant::now();
            let base_layers = &mut self.layers[..overlay_start];
            let count = base_layers.len();
            for layer in base_layers.iter_mut() {
                let layer_area = resolve_area(layer.as_ref(), area, &layout);
                if layer.layout_role() == LayoutRole::Fill {
                    layer.render(layer_area, surface, &render_ctx);
                } else {
                    let prepared = layer.prepare_render(layer_area, &render_ctx);
                    self.render_cache.compose(prepared, surface);
                }
            }
            log_run_phase(
                "compositor_layer",
                "base_batch",
                base_start.elapsed(),
                || format!("count={count} phase=base_serial"),
            );
        }

        // Phase 4: Info popup — on top of panels but below overlays.
        if render_ctx.config().auto_info {
            if let Some(info) = render_ctx.autoinfo() {
                let info_start = std::time::Instant::now();
                let mut info_copy = info.clone();
                info_copy.render(area, surface, &render_ctx);
                log_run_phase("compositor_layer", "autoinfo", info_start.elapsed(), || {
                    format!(
                        "id=autoinfo role=overlay area={}x{}+{},{}",
                        area.width, area.height, area.x, area.y
                    )
                });
            }
        }

        // Phase 5: Model floats — above editor/panels/autoinfo, below modal overlays.
        let float_count = render_ctx.model_float_count();
        if float_count > 0 {
            let floats_start = std::time::Instant::now();
            crate::ui::plugin_float::render_model_floats(area, surface, &render_ctx);
            log_run_phase(
                "compositor_layer",
                "model_floats",
                floats_start.elapsed(),
                || format!("count={float_count} phase=model_floats"),
            );
        }

        // Phase 5.5: Global status row — the reserved bottom row, rendered
        // BEFORE overlays so an active cmdline / picker / popup paints
        // over it. When there's no overlay, status_msg from anywhere in
        // the app lands here, full terminal width, regardless of which
        // panel has focus.
        if layout.global_status_row.height > 0 {
            render_global_status_row(layout.global_status_row, surface, &render_ctx);
        }

        // Phase 6: Overlay layers (pickers, popups, prompts) on top of everything.
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
            log_run_phase(
                "compositor_layer",
                "overlay_batch",
                overlay_start_time.elapsed(),
                || format!("count={count} phase=overlay_direct"),
            );
        }
    }

    pub fn cursor(&self, area: Rect, editor: &Editor) -> (Option<Position>, CursorKind) {
        let cursor_start = std::time::Instant::now();
        let layout = compute_panel_layout(area, editor);

        let (cursor_pos, cursor_kind) = {
            let mut result = (None, CursorKind::Hidden);
            let mut checked = 0usize;
            for layer in self.layers.iter().rev() {
                checked += 1;
                let layer_area = resolve_area(layer.as_ref(), area, &layout);
                let layer_cursor_start = std::time::Instant::now();
                let (position, kind) = layer.cursor(layer_area, editor);
                if let Some(pos) = position {
                    log::info!(
                        target: crate::ui::picker::PICKER_TRACE_TARGET,
                        "phase=cursor_source source_type={} source_id={} role={:?} focused={} layer_area={}x{}+{},{} pos={},{} kind={:?} checked={} elapsed_us={}",
                        layer.type_name(),
                        layer.id().unwrap_or("<none>"),
                        layer.layout_role(),
                        layer.is_focused(),
                        layer_area.width,
                        layer_area.height,
                        layer_area.x,
                        layer_area.y,
                        pos.col,
                        pos.row,
                        kind,
                        checked,
                        layer_cursor_start.elapsed().as_micros(),
                    );
                    result = (Some(pos), kind);
                    break;
                }
            }
            if result.0.is_none() {
                log::info!(
                    target: crate::ui::picker::PICKER_TRACE_TARGET,
                    "phase=cursor_source source_type=<none> checked={} area={}x{} elapsed_us={}",
                    checked,
                    area.width,
                    area.height,
                    cursor_start.elapsed().as_micros(),
                );
            }
            result
        };

        // Hide cursor if it falls inside the info popup area
        if let (Some(pos), _) = (cursor_pos, cursor_kind) {
            if editor.config().auto_info {
                if let Some(ref info) = editor.autoinfo {
                    let info_area = info.screen_area(area);
                    if info_area.contains(pos.col as u16, pos.row as u16) {
                        log::info!(
                            target: crate::ui::picker::PICKER_TRACE_TARGET,
                            "phase=cursor_hidden_by_autoinfo pos={},{} info_area={}x{}+{},{} elapsed_us={}",
                            pos.col,
                            pos.row,
                            info_area.width,
                            info_area.height,
                            info_area.x,
                            info_area.y,
                            cursor_start.elapsed().as_micros(),
                        );
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
            .and_then(|component| component.downcast_mut())
    }

    pub fn find_id<T: 'static>(&mut self, id: &'static str) -> Option<&mut T> {
        self.layers
            .iter_mut()
            .find(|component| component.id() == Some(id))
            .and_then(|component| component.downcast_mut())
    }

    pub fn find_picker(&mut self) -> Option<&mut dyn PickerComponent> {
        self.layers
            .iter_mut()
            .find(|component| component.id() == Some(crate::ui::picker::ID))
            .and_then(|component| component.as_picker_component())
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

pub(crate) fn apply_post_action(compositor: &mut Compositor, cx: &mut Context, action: PostAction) {
    match action {
        PostAction::PopLayer {
            model_layer,
            remember_picker,
        } => {
            if remember_picker {
                compositor.last_picker = compositor.pop();
            } else {
                compositor.pop();
            }
            if let Some(id) = model_layer {
                cx.editor.model.remove_layer(id);
            }
        }
        PostAction::RemoveById(id) => {
            compositor.remove(id);
        }
        PostAction::PushLayer(layer) => compositor.push(layer),
        PostAction::ReplaceOrPushLayer { id, layer } => {
            compositor.remove(id);
            compositor.push(layer);
        }
        PostAction::UpdateCompletionFilter(c) => {
            let editor_view = compositor.find::<crate::ui::EditorView>().unwrap();
            if let Some(completion) = &mut editor_view.completion {
                completion.update_filter(c);
                if completion.is_empty() || c.is_some_and(|c| !helix_core::chars::char_is_word(c)) {
                    editor_view.clear_completion(cx.editor);
                    if c.is_some() {
                        crate::handlers::completion::trigger_auto_completion(cx.editor, false);
                    }
                } else {
                    crate::ui::completion_ingress::request_incomplete_completion_list(
                        cx.editor,
                        cx.ingress.clone(),
                    );
                }
            }
        }
        PostAction::ClearCompletion => {
            let editor_view = compositor.find::<crate::ui::EditorView>().unwrap();
            editor_view.clear_completion(cx.editor);
        }
        PostAction::ShowCommandPalette { register, count } => {
            crate::commands::show_command_palette(compositor, cx, register, count);
        }
        PostAction::RestoreLastPicker => {
            if let Some(picker) = compositor.last_picker.take() {
                compositor.push(picker);
            } else {
                cx.editor.set_error("no last picker")
            }
        }
        PostAction::ReplayKeys {
            keys,
            count,
            pop_macro_replaying,
        } => {
            for _ in 0..count {
                for key in &keys {
                    compositor.handle_event(&Event::Key(*key), cx);
                }
            }
            if pop_macro_replaying {
                cx.editor.macro_replaying.pop();
            }
        }
        PostAction::Batch(actions) => {
            for action in actions {
                apply_post_action(compositor, cx, action);
            }
        }
    }
}

// Downcasting via trait upcasting (stable since Rust 1.76).
// `Component: Any` allows `&dyn Component` → `&dyn Any` coercion,
// eliminating the need for a separate `AnyComponent` trait.

use std::any::Any;

impl dyn Component {
    /// Attempts to downcast `self` to a concrete type.
    pub fn downcast_ref<T: Any>(&self) -> Option<&T> {
        (self as &dyn Any).downcast_ref()
    }

    /// Attempts to downcast `self` to a concrete type.
    pub fn downcast_mut<T: Any>(&mut self) -> Option<&mut T> {
        (self as &mut dyn Any).downcast_mut()
    }

    /// Attempts to downcast `Box<Self>` to a concrete type.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use helix_term::{ui::Text, compositor::Component};
    /// let boxed: Box<dyn Component> = Box::new(Text::new("text".to_string()));
    /// let text: Box<Text> = match boxed.downcast() {
    ///     Ok(text) => text,
    ///     Err(_) => unreachable!("boxed component is Text"),
    /// };
    /// ```
    pub fn downcast<T: Any>(self: Box<Self>) -> Result<Box<T>, Box<Self>> {
        if (self.as_ref() as &dyn Any).is::<T>() {
            // Upcast Box<dyn Component> to Box<dyn Any>, then downcast.
            let boxed_any: Box<dyn Any> = self;
            Ok(boxed_any.downcast().unwrap())
        } else {
            Err(self)
        }
    }

    /// Checks if this component is of type `T`.
    pub fn is<T: Any>(&self) -> bool {
        (self as &dyn Any).is::<T>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_swap::ArcSwap;
    use helix_view::model::AssistantModel;
    use std::path::PathBuf;
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
            helix_runtime::test::runtime(),
            handlers,
        )
    }

    #[tokio::test]
    async fn file_picker_renders_workspace_root() {
        let mut editor = test_editor(151, 43);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("helix-term has workspace parent")
            .to_path_buf();
        let runtime = helix_runtime::test::runtime();
        let (ingress, _rx) = crate::runtime::RuntimeIngress::channel(runtime.work().clone());
        let mut picker = crate::ui::file_picker(&editor, root, ingress.clone());
        picker.required_size((151, 43));
        picker.sync(&mut editor);

        let area = Rect::new(0, 0, 151, 43);
        let render_ctx = RenderContext::new(&editor, ingress, editor.redraw_handle(), None);

        let mut ratatui_surface =
            crate::render::CellSurface::empty(tui::ratatui::to_ratatui_rect(area));
        picker.render(area, &mut ratatui_surface, &render_ctx);
    }

    #[tokio::test]
    async fn component_renders_to_ratatui_with_native_hook() {
        struct Probe;

        impl Component for Probe {
            fn render(
                &mut self,
                area: Rect,
                surface: &mut crate::render::CellSurface,
                _cx: &RenderContext,
            ) {
                surface.set_string(area.x, area.y, "r", tui::ratatui::style::Style::default());
            }
        }

        let editor = test_editor(4, 2);
        let runtime = helix_runtime::test::runtime();
        let (ingress, _rx) = crate::runtime::RuntimeIngress::channel(runtime.work().clone());
        let render_ctx = RenderContext::new(&editor, ingress, editor.redraw_handle(), None);
        let area = Rect::new(0, 0, 4, 2);
        let mut surface = crate::render::CellSurface::empty(tui::ratatui::to_ratatui_rect(area));
        let mut probe = Probe;

        probe.render(area, &mut surface, &render_ctx);

        assert_eq!(surface[(0, 0)].symbol(), "r");
    }

    #[tokio::test]
    async fn compositor_composes_native_prepared_ratatui_render() {
        struct Probe;

        impl Component for Probe {
            fn render(
                &mut self,
                area: Rect,
                surface: &mut crate::render::CellSurface,
                _cx: &RenderContext,
            ) {
                surface.set_string(area.x, area.y, "r", tui::ratatui::style::Style::default());
            }

            fn prepare_render(&mut self, area: Rect, _ctx: &RenderContext) -> PreparedRender {
                let mut output = RenderOutput::new(area);
                output.surface_mut().set_string(
                    area.x,
                    area.y,
                    "n",
                    tui::ratatui::style::Style::default(),
                );
                PreparedRender::ready(output)
            }
        }

        let editor = test_editor(4, 2);
        let runtime = helix_runtime::test::runtime();
        let (ingress, _rx) = crate::runtime::RuntimeIngress::channel(runtime.work().clone());
        let render_ctx = RenderContext::new(&editor, ingress, editor.redraw_handle(), None);
        let area = Rect::new(0, 0, 4, 2);
        let mut surface = crate::render::CellSurface::empty(tui::ratatui::to_ratatui_rect(area));
        let mut compositor = Compositor::new(area);
        let mut probe = Probe;

        let prepared = probe.prepare_render(area, &render_ctx);
        compositor
            .render_cache
            .compose_batch([prepared], &mut surface);

        assert_eq!(surface[(0, 0)].symbol(), "n");
    }

    #[tokio::test]
    async fn compositor_render_frame_returns_cell_output() {
        struct Probe;

        impl Component for Probe {
            fn render(
                &mut self,
                area: Rect,
                surface: &mut crate::render::CellSurface,
                _cx: &RenderContext,
            ) {
                surface.set_string(area.x, area.y, "f", tui::ratatui::style::Style::default());
            }
        }

        let mut editor = test_editor(4, 2);
        let runtime = helix_runtime::test::runtime();
        let (ingress, _rx) = crate::runtime::RuntimeIngress::channel(runtime.work().clone());
        let (plugin_events, _plugin_events_rx) = helix_runtime::channel(1);
        let idle_reset = crate::runtime::IdleResetGate::new().handle();
        let redraw = editor.redraw_handle();
        let notifier = crate::handlers::local::Notifier {
            redraw: redraw.clone(),
            plugin_events,
        };
        let mut exit_tasks = crate::runtime::ExitTaskSet::default();
        let exit_task_work = editor.work();
        let area = Rect::new(0, 0, 4, 2);
        let mut compositor = Compositor::new(area);
        compositor.push(Box::new(Probe));

        let mut cx = Context::new(
            &mut editor,
            &mut exit_tasks,
            exit_task_work,
            notifier,
            ingress,
            idle_reset,
            None,
        );

        let output = compositor.render_frame(area, &mut cx);

        assert_eq!(output.area(), area);
        assert_eq!(output.surface()[(0, 0)].symbol(), "f");
    }

    #[tokio::test]
    async fn apply_post_action_dispatches_layer_requests() {
        struct Probe(&'static str);

        impl Component for Probe {
            fn id(&self) -> Option<&str> {
                Some(self.0)
            }

            fn render(
                &mut self,
                _area: Rect,
                _surface: &mut crate::render::CellSurface,
                _cx: &RenderContext,
            ) {
            }
        }

        let mut editor = test_editor(20, 10);
        editor.macro_replaying.push('@');
        let runtime = helix_runtime::test::runtime();
        let (ingress, _rx) = crate::runtime::RuntimeIngress::channel(runtime.work().clone());
        let (plugin_events, _plugin_events_rx) = helix_runtime::channel(1);
        let idle_reset = crate::runtime::IdleResetGate::new().handle();
        let redraw = editor.redraw_handle();
        let notifier = crate::handlers::local::Notifier {
            redraw: redraw.clone(),
            plugin_events,
        };
        let mut exit_tasks = crate::runtime::ExitTaskSet::default();
        let exit_task_work = editor.work();
        let area = Rect::new(0, 0, 20, 10);
        let mut compositor = Compositor::new(area);
        let mut cx = Context::new(
            &mut editor,
            &mut exit_tasks,
            exit_task_work,
            notifier,
            ingress,
            idle_reset,
            None,
        );

        apply_post_action(
            &mut compositor,
            &mut cx,
            PostAction::Batch(vec![
                PostAction::PushLayer(Box::new(Probe("old"))),
                PostAction::ReplaceOrPushLayer {
                    id: "old",
                    layer: Box::new(Probe("new")),
                },
                PostAction::RemoveById("missing"),
                PostAction::ReplayKeys {
                    keys: Vec::new(),
                    count: 2,
                    pop_macro_replaying: true,
                },
            ]),
        );

        assert!(compositor.find_id::<Probe>("old").is_none());
        assert!(compositor.find_id::<Probe>("new").is_some());
        assert!(cx.editor.macro_replaying.is_empty());

        apply_post_action(
            &mut compositor,
            &mut cx,
            PostAction::PopLayer {
                model_layer: None,
                remember_picker: true,
            },
        );
        assert_eq!(compositor.layer_count(), 0);
        assert!(compositor.last_picker.is_some());

        apply_post_action(&mut compositor, &mut cx, PostAction::RestoreLastPicker);
        assert!(compositor.find_id::<Probe>("new").is_some());
    }

    #[tokio::test]
    async fn panel_layout_right_percent35_splits_correctly() {
        let mut editor = test_editor(120, 40);
        editor.model.insert_panel(
            "Assistant",
            Box::new(AssistantModel::default()),
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
        // Bottom row reserved for the global status line; side panels
        // underlap it so their internal footer/status row aligns with the
        // editor statusline one row above the global message row.
        assert_eq!(layout.editor_area.height, 39);
        assert_eq!(panel_rect.height, 40);
        assert_eq!(layout.global_status_row, Rect::new(0, 39, 120, 1));
    }

    #[tokio::test]
    async fn panel_layout_side_panel_footer_aligns_with_editor_statusline() {
        let mut editor = test_editor(120, 40);
        editor.model.insert_panel(
            "Assistant",
            Box::new(AssistantModel::default()),
            PanelSide::Right,
            PanelSize::Percent(35),
        );
        let layout = compute_panel_layout(Rect::new(0, 0, 120, 40), &editor);
        let (_, panel_rect) = &layout.panel_areas[0];

        // AssistantPanel lays out [header, content, input, footer, error].
        // The global status row paints over the error row, so the footer must
        // land on the editor's own statusline baseline.
        let editor_statusline_row = layout.editor_area.bottom().saturating_sub(1);
        let panel_footer_row = panel_rect.bottom().saturating_sub(2);
        assert_eq!(panel_footer_row, editor_statusline_row);
        assert_eq!(
            layout.global_status_row.y,
            panel_rect.bottom().saturating_sub(1)
        );
    }

    #[tokio::test]
    async fn panel_layout_right_hidden_when_narrow() {
        let mut editor = test_editor(50, 24);
        editor.model.insert_panel(
            "Assistant",
            Box::new(AssistantModel::default()),
            PanelSide::Right,
            PanelSize::Percent(35),
        );
        let layout = compute_panel_layout(Rect::new(0, 0, 50, 24), &editor);

        // Terminal too narrow (≤60) — panel should be skipped
        assert!(layout.panel_areas.is_empty());
        assert_eq!(layout.editor_area.width, 50);
        // Bottom row reserved for the global status line.
        assert_eq!(layout.editor_area.height, 23);
    }

    #[tokio::test]
    async fn panel_layout_skips_hidden_panels() {
        let mut editor = test_editor(120, 40);
        let panel_id = editor.model.insert_panel(
            "Assistant",
            Box::new(AssistantModel::default()),
            PanelSide::Right,
            PanelSize::Percent(35),
        );
        assert_eq!(editor.model.toggle_panel(panel_id), Some(false));

        let layout = compute_panel_layout(Rect::new(0, 0, 120, 40), &editor);

        assert!(layout.panel_areas.is_empty());
        // Bottom row reserved for the global status line — editor gets the rest.
        assert_eq!(layout.editor_area, Rect::new(0, 0, 120, 39));
        assert_eq!(layout.global_status_row, Rect::new(0, 39, 120, 1));
    }

    #[tokio::test]
    async fn panel_layout_uses_panel_id_not_string() {
        let mut editor = test_editor(120, 40);
        let panel_id = editor.model.insert_panel(
            "Assistant",
            Box::new(AssistantModel::default()),
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
            "Assistant",
            Box::new(AssistantModel::default()),
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
