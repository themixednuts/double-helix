//! Plugin panel — a Component that delegates rendering to a Lua callback.
//!
//! When `render()` is called, it wraps the real `Surface` in a `TermDrawSurface`
//! (implementing `helix_plugin::types::DrawSurface`), sets up the thread-local
//! render context, and invokes the plugin's Lua render callback.

use crate::compositor::{Component, Context, Event, EventResult, RenderContext};
use helix_plugin::mlua;
use helix_view::graphics::{Rect, Style};
use helix_view::model::PanelId;
use tui::buffer::Buffer as Surface;

/// Component that bridges Lua plugin rendering with the compositor.
pub struct PluginPanel {
    plugin_name: String,
    panel_id: String,
    render_callback_id: u64,
    event_callback_id: Option<u64>,
    model_panel_id: Option<PanelId>,
    // Leaked string for Component::id() — bounded by panel count.
    component_id: &'static str,
}

impl PluginPanel {
    pub fn new(
        plugin_name: String,
        panel_id: String,
        render_callback_id: u64,
        event_callback_id: Option<u64>,
    ) -> Self {
        let component_id: &'static str =
            Box::leak(format!("plugin_panel:{panel_id}").into_boxed_str());
        Self {
            plugin_name,
            panel_id,
            render_callback_id,
            event_callback_id,
            model_panel_id: None,
            component_id,
        }
    }

    pub fn set_model_panel_id(&mut self, id: PanelId) {
        self.model_panel_id = Some(id);
    }

    pub fn plugin_panel_id(&self) -> &str {
        &self.panel_id
    }
}

impl Component for PluginPanel {
    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &RenderContext) {
        let Some(ref pm) = cx.plugin_manager else {
            return;
        };

        let engine = pm.engine();
        let engine = engine.read();
        let lua = engine.lua();

        // Retrieve the render callback from the Lua registry.
        let callbacks = engine.ui_callbacks();
        let callbacks = callbacks.read();
        let key = (self.plugin_name.clone(), self.render_callback_id);
        let Some(callback_ref) = callbacks.get(&key) else {
            let style = cx.editor.theme.get("error");
            surface.set_stringn(
                area.x,
                area.y,
                "Plugin render error",
                area.width as usize,
                style,
            );
            return;
        };

        let Ok(callback) = lua.registry_value::<mlua::Function>(callback_ref) else {
            return;
        };

        let Ok(area_table) = helix_plugin::lua::api::surface::rect_to_table(lua, area) else {
            return;
        };
        let Ok(lua_surface) = lua.create_userdata(helix_plugin::lua::api::surface::LuaSurface)
        else {
            return;
        };

        // Wrap the real surface in DrawSurface and call the Lua function.
        // Use a raw pointer for the theme to avoid borrow conflict with cx.editor.
        // Safety: theme lives as long as editor, and we're single-threaded during render.
        let theme_ptr = &cx.editor.theme as *const helix_view::Theme;
        let theme = unsafe { &*theme_ptr };
        let mut wrapper = TermDrawSurface { surface };
        helix_plugin::lua::with_render_context(&mut wrapper, theme, || {
            helix_plugin::lua::with_editor_context_ref(cx.editor, || {
                if let Err(e) = callback.call::<()>((area_table, lua_surface)) {
                    log::error!("Plugin panel render error ({}): {}", self.panel_id, e);
                }
            });
        });
    }

    fn handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        let Some(event_callback_id) = self.event_callback_id else {
            return EventResult::Ignored(None);
        };

        let Event::Key(key_event) = event else {
            return EventResult::Ignored(None);
        };

        let Some(ref pm) = cx.plugin_manager else {
            return EventResult::Ignored(None);
        };

        let engine = pm.engine();
        let engine = engine.read();
        let lua = engine.lua();

        let callbacks = engine.ui_callbacks();
        let callbacks = callbacks.read();
        let key = (self.plugin_name.clone(), event_callback_id);
        let Some(callback_ref) = callbacks.get(&key) else {
            return EventResult::Ignored(None);
        };

        let Ok(callback) = lua.registry_value::<mlua::Function>(callback_ref) else {
            return EventResult::Ignored(None);
        };

        // Build event table.
        let Ok(event_table) = lua.create_table() else {
            return EventResult::Ignored(None);
        };
        let _ = event_table.set("key", format!("{key_event}"));

        let consumed = helix_plugin::lua::with_editor_context(cx.editor, || {
            matches!(callback.call::<Option<bool>>(event_table), Ok(Some(true)))
        });

        if consumed {
            EventResult::Consumed(None)
        } else {
            EventResult::Ignored(None)
        }
    }

    fn id(&self) -> Option<&'static str> {
        Some(self.component_id)
    }

    fn panel_id(&self) -> Option<helix_view::model::PanelId> {
        self.model_panel_id
    }
}

// ─── TermDrawSurface ─────────────────────────────────────────────────

/// Concrete implementation of `DrawSurface` wrapping a `tui::buffer::Buffer`.
struct TermDrawSurface<'a> {
    surface: &'a mut Surface,
}

impl helix_plugin::types::DrawSurface for TermDrawSurface<'_> {
    fn set_string(&mut self, x: u16, y: u16, text: &str, style: Style) {
        self.surface.set_string(x, y, text, style);
    }

    fn set_stringn(&mut self, x: u16, y: u16, text: &str, max_width: usize, style: Style) {
        self.surface.set_stringn(x, y, text, max_width, style);
    }

    fn clear_with(&mut self, area: Rect, style: Style) {
        self.surface.clear_with(area, style);
    }

    fn set_style(&mut self, area: Rect, style: Style) {
        self.surface.set_style(area, style);
    }

    fn header(&mut self, area: Rect, title: &str, style: Style) {
        crate::widgets::header(self.surface, area, title, style);
    }

    fn header_with_counts(
        &mut self,
        area: Rect,
        title: &str,
        current: usize,
        total: usize,
        style: Style,
    ) {
        crate::widgets::header_with_counts(self.surface, area, title, current, total, style);
    }

    fn hdivider(&mut self, area: Rect, style: Style) {
        crate::widgets::hdivider(self.surface, area, style);
    }

    fn vdivider(&mut self, area: Rect, style: Style) {
        crate::widgets::vdivider(self.surface, area, style);
    }

    fn text_input(
        &mut self,
        area: Rect,
        text: &str,
        cursor: usize,
        style: Style,
        cursor_style: Style,
    ) -> (u16, u16) {
        let state =
            crate::widgets::text_input(self.surface, area, text, cursor, style, cursor_style);
        (state.cursor_x, state.cursor_y)
    }

    fn scrollbar(
        &mut self,
        area: Rect,
        total: usize,
        offset: usize,
        visible: usize,
        thumb_style: Style,
        track_symbol: Option<&str>,
        track_style: Style,
    ) {
        let mut sb =
            crate::widgets::Scrollbar::new(total, offset, visible).thumb_style(thumb_style);
        if let Some(sym) = track_symbol {
            sb = sb.track(Box::leak(sym.to_owned().into_boxed_str()), track_style);
        } else {
            sb = sb.track(" ", track_style);
        }
        sb.render(area, self.surface);
    }
}
