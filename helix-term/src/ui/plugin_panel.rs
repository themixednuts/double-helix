//! Plugin panel backed by retained, host-owned render nodes.

use crate::compositor::{Component, RenderContext};
use helix_plugin_api::PanelHandle;
use helix_plugin_editor::adapt;
use helix_view::graphics::Rect;
use helix_view::model::{FocusTarget, PanelId};
use helix_view::traits::Focusable;

pub(crate) fn component_id(panel: PanelHandle) -> String {
    format!("plugin_panel:{}", panel.raw().get())
}

/// Component that bridges Lua plugin rendering with the compositor.
pub struct PluginPanel {
    model_panel_id: PanelId,
    focused: bool,
    component_id: String,
    content: std::sync::Arc<[helix_plugin_api::requests::UiRenderNode]>,
}

impl PluginPanel {
    pub fn new(
        panel: PanelHandle,
        model_panel_id: PanelId,
        content: std::sync::Arc<[helix_plugin_api::requests::UiRenderNode]>,
    ) -> Self {
        Self {
            model_panel_id,
            focused: false,
            component_id: component_id(panel),
            content,
        }
    }

    pub fn from_editor(
        editor: &helix_view::Editor,
        panel: PanelHandle,
        content: std::sync::Arc<[helix_plugin_api::requests::UiRenderNode]>,
    ) -> Option<Self> {
        let model_panel_id = adapt::resolve_panel(&editor.model, panel).ok()?;
        Some(Self::new(panel, model_panel_id, content))
    }

    pub fn set_content(
        &mut self,
        content: std::sync::Arc<[helix_plugin_api::requests::UiRenderNode]>,
    ) {
        self.content = content;
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
    fn sync(&mut self, _viewport: Rect, editor: &mut helix_view::Editor) {
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

    fn prepare_render(&mut self, area: Rect, cx: &RenderContext) -> crate::render::PreparedRender {
        let content = std::sync::Arc::clone(&self.content);
        let theme = cx.theme_arc();
        crate::render::PreparedRender::deferred(move |cancellation| {
            let mut output = crate::render::RenderOutput::sparse(area);
            if !cancellation.is_cancelled() {
                paint_plugin_panel(output.surface_mut(), area, &content, &theme);
            }
            output
        })
    }

    fn id(&self) -> Option<&str> {
        Some(&self.component_id)
    }

    fn layout_role(&self) -> crate::compositor::LayoutRole {
        crate::compositor::LayoutRole::Docked
    }

    fn panel_id(&self) -> Option<helix_view::model::PanelId> {
        Some(self.model_panel_id)
    }

    crate::component_traits!(focusable);
}

fn paint_plugin_panel(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    content: &[helix_plugin_api::requests::UiRenderNode],
    theme: &helix_view::Theme,
) {
    crate::ui::plugin_render::render_retained_nodes(surface, area, content, |scope| {
        theme.get(scope)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_swap::ArcSwap;
    use helix_view::model::{AssistantModel, PanelSide, PanelSize};
    use std::num::NonZeroU64;
    use std::sync::Arc;

    fn test_editor(width: u16, height: u16) -> helix_view::Editor {
        let theme_loader = helix_view::theme::Loader::new(&[]);
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
        let panel = PluginPanel::new(
            PanelHandle::from_raw(NonZeroU64::new(1).unwrap()),
            panel_id,
            Arc::from([]),
        );

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
        let panel_handle = adapt::panel_handle(panel_id);

        let panel = PluginPanel::from_editor(&editor, panel_handle, Arc::from([]))
            .expect("panel component");

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
        let mut panel = PluginPanel::new(
            PanelHandle::from_raw(NonZeroU64::new(1).unwrap()),
            panel_id,
            Arc::from([]),
        );

        editor.model.focus_panel(panel_id);
        panel.sync(Rect::new(0, 0, 120, 40), &mut editor);
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
        panel.sync(Rect::new(0, 0, 120, 40), &mut editor);
        assert!(!Focusable::is_focused(&panel));

        panel.set_focused(true);
        editor.model.focus = FocusTarget::Editor;
        panel.sync(Rect::new(0, 0, 120, 40), &mut editor);
        assert!(Focusable::is_focused(&panel));

        assert_eq!(editor.model.toggle_panel(panel_id), Some(false));
        panel.sync(Rect::new(0, 0, 120, 40), &mut editor);
        assert!(!Focusable::is_focused(&panel));
    }
}
