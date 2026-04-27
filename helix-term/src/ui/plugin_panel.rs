//! Plugin panel — a Component that delegates rendering to a Lua callback.
//!
//! When `render()` is called, it wraps the real `Surface` in a `TermDrawSurface`
//! (implementing `helix_plugin::types::DrawSurface`), sets up the thread-local
//! render context, and invokes the plugin's Lua render callback.

use crate::compositor::{Component, Context, Event, EventResult, RenderContext};
use helix_plugin::contract::{adapt, PanelHandle};
use helix_plugin::mlua;
use helix_view::graphics::{Rect, Style};
use helix_view::model::{FocusTarget, PanelId};
use helix_view::traits::Focusable;
use tui::buffer::Buffer as Surface;

pub(crate) fn component_id(panel: PanelHandle) -> String {
    format!("plugin_panel:{}", panel.raw().get())
}

/// Component that bridges Lua plugin rendering with the compositor.
pub struct PluginPanel {
    panel: PanelHandle,
    model_panel_id: PanelId,
    focused: bool,
    // Leaked string for Component::id() - bounded by panel count.
    component_id: &'static str,
}

impl PluginPanel {
    pub fn new(panel: PanelHandle, model_panel_id: PanelId) -> Self {
        let component_id: &'static str = Box::leak(component_id(panel).into_boxed_str());
        Self {
            panel,
            model_panel_id,
            focused: false,
            component_id,
        }
    }

    pub fn from_editor(editor: &helix_view::Editor, panel: PanelHandle) -> Option<Self> {
        let model_panel_id = adapt::resolve_panel(&editor.model, panel).ok()?;
        Some(Self::new(panel, model_panel_id))
    }
}

impl Focusable for PluginPanel {
    fn is_focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

impl Component for PluginPanel {
    fn sync(&mut self, editor: &mut helix_view::Editor) {
        let visible = editor
            .model
            .panels
            .get(self.model_panel_id)
            .is_some_and(|panel| panel.visible);
        if !visible {
            self.focused = false;
            return;
        }

        match editor.model.focus {
            FocusTarget::Panel(id) => {
                self.focused = id == self.model_panel_id;
            }
            FocusTarget::Editor => {}
            FocusTarget::Layer(_) | FocusTarget::Float(_) => {
                self.focused = false;
            }
        }
    }

    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &RenderContext) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let Some(ref pm) = cx.plugin_manager else {
            return;
        };

        let engine = pm.engine();
        let engine = engine.read();
        let lua = engine.lua();

        let panel_callbacks = engine.panel_callbacks();
        let panel_callbacks = panel_callbacks.read();
        let Some(callbacks) = panel_callbacks.get(&self.panel) else {
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

        let ui_callbacks = engine.ui_callbacks();
        let ui_callbacks = ui_callbacks.read();
        let key = helix_plugin::types::PluginCallbackKey::new(
            callbacks.plugin_name.clone(),
            callbacks.render_callback_id,
        );
        let Some(callback_ref) = ui_callbacks.get(&key) else {
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

        let Ok(area_table) = helix_plugin::lua::api::facade::rect_to_table(lua, area) else {
            return;
        };
        let Ok(lua_surface) = lua.create_userdata(helix_plugin::lua::api::facade::LuaSurface)
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
                if let Err(e) =
                    helix_plugin::lua::with_current_plugin_name(lua, &callbacks.plugin_name, || {
                        callback.call::<()>((area_table, lua_surface))
                    })
                {
                    log::error!("Plugin panel render error ({}): {}", self.panel, e);
                }
            });
        });
    }

    fn handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        if !self.focused {
            return EventResult::Ignored(None);
        }

        let Some(ref pm) = cx.plugin_manager else {
            return EventResult::Ignored(None);
        };

        let Event::Key(key_event) = event else {
            return EventResult::Ignored(None);
        };

        let engine = pm.engine();
        let engine = engine.read();
        let panel_callbacks = engine.panel_callbacks();
        let panel_callbacks = panel_callbacks.read();
        let Some(callbacks) = panel_callbacks.get(&self.panel) else {
            return EventResult::Ignored(None);
        };
        let Some(event_callback_id) = callbacks.event_callback_id else {
            return EventResult::Ignored(None);
        };

        let lua = engine.lua();

        let ui_callbacks = engine.ui_callbacks();
        let ui_callbacks = ui_callbacks.read();
        let key = helix_plugin::types::PluginCallbackKey::new(
            callbacks.plugin_name.clone(),
            event_callback_id,
        );
        let Some(callback_ref) = ui_callbacks.get(&key) else {
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
            match helix_plugin::lua::with_current_plugin_name(lua, &callbacks.plugin_name, || {
                callback.call::<Option<bool>>(event_table)
            }) {
                Ok(Some(true)) => true,
                Ok(_) => false,
                Err(err) => {
                    log::error!("Plugin panel event error ({}): {}", self.panel, err);
                    false
                }
            }
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

    fn layout_role(&self) -> crate::compositor::LayoutRole {
        crate::compositor::LayoutRole::Docked
    }

    fn panel_id(&self) -> Option<helix_view::model::PanelId> {
        Some(self.model_panel_id)
    }

    crate::component_traits!(focusable);
}

// ─── TermDrawSurface ─────────────────────────────────────────────────

/// Concrete implementation of `DrawSurface` wrapping a `tui::buffer::Buffer`.
pub(crate) struct TermDrawSurface<'a> {
    pub(crate) surface: &'a mut Surface,
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

#[cfg(test)]
mod tests {
    use super::*;
    use arc_swap::ArcSwap;
    use helix_view::model::{AssistantModel, PanelSide, PanelSize};
    use std::num::NonZeroU64;
    use std::sync::Arc;

    fn test_editor(width: u16, height: u16) -> helix_view::Editor {
        let theme_loader = helix_view::theme::Loader::new(helix_loader::runtime_dirs());
        let syn_loader = helix_core::config::default_lang_loader();
        let config = helix_view::editor::Config::default();
        let config = Arc::new(ArcSwap::from_pointee(config));
        let handlers = helix_view::handlers::Handlers::dummy();
        helix_view::Editor::new(
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
    async fn plugin_panel_is_docked() {
        let mut editor = test_editor(120, 40);
        let panel_id = editor.model.insert_panel(
            "Plugin",
            Box::new(AssistantModel::default()),
            PanelSide::Right,
            PanelSize::Percent(35),
        );
        let panel = PluginPanel::new(PanelHandle::from_raw(NonZeroU64::new(1).unwrap()), panel_id);

        assert_eq!(panel.layout_role(), crate::compositor::LayoutRole::Docked);
        assert_eq!(panel.panel_id(), Some(panel_id));
    }

    #[tokio::test]
    async fn plugin_panel_from_editor_resolves_handle() {
        let mut editor = test_editor(120, 40);
        let panel_id = editor.model.insert_panel(
            "Plugin",
            Box::new(AssistantModel::default()),
            PanelSide::Right,
            PanelSize::Percent(35),
        );
        let panel_handle = helix_plugin::contract::adapt::panel_handle(panel_id);

        let panel = PluginPanel::from_editor(&editor, panel_handle).expect("panel component");

        assert_eq!(panel.panel, panel_handle);
        assert_eq!(panel.panel_id(), Some(panel_id));
    }

    #[tokio::test]
    async fn plugin_panel_sync_tracks_focus_and_visibility() {
        let mut editor = test_editor(120, 40);
        let panel_id = editor.model.insert_panel(
            "Plugin",
            Box::new(AssistantModel::default()),
            PanelSide::Right,
            PanelSize::Percent(35),
        );
        let mut panel =
            PluginPanel::new(PanelHandle::from_raw(NonZeroU64::new(1).unwrap()), panel_id);

        editor.model.focus_panel(panel_id);
        panel.sync(&mut editor);
        assert!(Focusable::is_focused(&panel));

        let float_id = editor.model.create_float(
            Box::new(helix_view::model::TextFloatModel::default()),
            helix_view::model::Placement::Centered {
                width: 20,
                height: 5,
            },
            None,
            true,
            true,
        );
        assert_eq!(editor.model.focus, FocusTarget::Float(float_id));
        panel.sync(&mut editor);
        assert!(!Focusable::is_focused(&panel));

        panel.set_focused(true);
        editor.model.focus = FocusTarget::Editor;
        panel.sync(&mut editor);
        assert!(Focusable::is_focused(&panel));

        assert_eq!(editor.model.toggle_panel(panel_id), Some(false));
        panel.sync(&mut editor);
        assert!(!Focusable::is_focused(&panel));
    }
}
