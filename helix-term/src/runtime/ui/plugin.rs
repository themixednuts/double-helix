use crate::{
    compositor::{Component, Compositor, Event, EventResult, RenderContext},
    plugin_registry::{PluginPanelKeyRoute, PluginUiCallback},
    runtime::ui::command::PluginCommand,
};
use helix_plugin_api::requests::{NotifyLevel, PanelSizeSpec};
use helix_plugin_api::{DynamicValue, PanelHandle};
use helix_plugin_editor::adapt;
use helix_view::model::PanelSize;
use helix_view::traits::Focusable;

fn deliver_plugin_ui_callback(
    _context: &mut crate::compositor::Context<'_>,
    callback: &PluginUiCallback,
    value: DynamicValue,
) {
    callback.send(value);
}

struct RoutedPluginPanel {
    inner: crate::ui::plugin_panel::PluginPanel,
    host_key_events: Option<PluginPanelKeyRoute>,
}

impl RoutedPluginPanel {
    fn from_editor(
        editor: &helix_view::Editor,
        panel: PanelHandle,
        content: std::sync::Arc<[helix_plugin_api::requests::UiRenderNode]>,
        host_key_events: Option<PluginPanelKeyRoute>,
    ) -> Option<Self> {
        Some(Self {
            inner: crate::ui::plugin_panel::PluginPanel::from_editor(editor, panel, content)?,
            host_key_events,
        })
    }

    fn set_content(&mut self, content: std::sync::Arc<[helix_plugin_api::requests::UiRenderNode]>) {
        self.inner.set_content(content);
    }
}

impl Component for RoutedPluginPanel {
    fn handle_event(
        &mut self,
        event: &Event,
        context: &mut crate::compositor::Context,
    ) -> EventResult {
        if Focusable::is_focused(&self.inner) {
            if let (Some(route), Event::Key(key_event)) = (&self.host_key_events, event) {
                route.dispatch(format!("{key_event}"));
                return EventResult::Consumed(None);
            }
        }
        self.inner.handle_event(event, context)
    }

    fn sync(&mut self, viewport: helix_view::graphics::Rect, editor: &mut helix_view::Editor) {
        self.inner.sync(viewport, editor);
    }

    fn prepare_render(
        &mut self,
        area: helix_view::graphics::Rect,
        context: &RenderContext,
    ) -> crate::render::PreparedRender {
        self.inner.prepare_render(area, context)
    }

    fn id(&self) -> Option<&str> {
        self.inner.id()
    }

    fn layout_role(&self) -> crate::compositor::LayoutRole {
        self.inner.layout_role()
    }

    fn panel_id(&self) -> Option<helix_view::model::PanelId> {
        self.inner.panel_id()
    }

    fn is_focused(&self) -> bool {
        Focusable::is_focused(&self.inner)
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(&mut self.inner)
    }
}

fn plugin_panel_id(
    editor: &helix_view::Editor,
    panel: PanelHandle,
) -> Option<helix_view::model::PanelId> {
    match adapt::resolve_panel(&editor.model, panel) {
        Ok(id) => Some(id),
        Err(err) => {
            log::warn!("dropping stale plugin panel UI command for {panel}: {err}");
            None
        }
    }
}

pub(crate) fn apply_plugin_command(
    compositor: &mut Compositor,
    context: &mut crate::compositor::Context<'_>,
    cmd: PluginCommand,
) {
    let editor = &mut *context.editor;
    let ingress = context.ingress.clone();
    match cmd {
        PluginCommand::SetTheme { theme, completion } => {
            editor.set_theme(theme);
            if let Err(error) =
                completion.complete_foreground(Ok(helix_plugin_api::PluginTaskResult::Unit))
            {
                editor.set_error(error.to_string());
            }
        }
        PluginCommand::RunCommand {
            request,
            completion,
        } => {
            let result = compositor
                .find::<crate::ui::EditorView>()
                .ok_or_else(|| helix_plugin_api::ContractError::not_found("editor component"))
                .and_then(|editor_view| editor_view.execute_plugin_command(context, &request));
            match result {
                Ok(actions) => {
                    for action in actions {
                        crate::compositor::apply_post_action(compositor, context, action);
                    }
                    if let Err(error) =
                        completion.complete_foreground(Ok(helix_plugin_api::PluginTaskResult::Unit))
                    {
                        context.editor.set_error(error.to_string());
                    }
                }
                Err(error) => {
                    if let Err(error) = completion.complete_foreground(Err(error)) {
                        context.editor.set_error(error.to_string());
                    }
                }
            }
        }
        PluginCommand::SetKeymap {
            keymap,
            contribution,
        } => {
            if let Some(editor_view) = compositor.find::<crate::ui::EditorView>() {
                editor_view.keymaps.set_contribution(keymap, contribution);
                let effective = editor_view.keymaps.map();
                editor.set_modal_keymaps(crate::keymap::to_component_modal_keymaps(&effective));
                editor.set_semantic_modal_keymaps(crate::keymap::to_semantic_modal_keymaps(
                    &effective,
                ));
            }
        }
        PluginCommand::RemoveKeymap { keymap } => {
            if let Some(editor_view) = compositor.find::<crate::ui::EditorView>() {
                if editor_view.keymaps.remove_contribution(keymap) {
                    let effective = editor_view.keymaps.map();
                    editor.set_modal_keymaps(crate::keymap::to_component_modal_keymaps(&effective));
                    editor.set_semantic_modal_keymaps(crate::keymap::to_semantic_modal_keymaps(
                        &effective,
                    ));
                }
            }
        }
        PluginCommand::Notify { level, message } => match level {
            NotifyLevel::Info => editor.set_status(message),
            NotifyLevel::Warn => editor.set_status(format!("Warning: {message}")),
            NotifyLevel::Error => editor.set_error(message),
        },
        PluginCommand::Prompt { request, callback } => {
            let prompt = crate::ui::Prompt::new(
                request.message.into(),
                None,
                crate::ui::completers::none,
                move |cx, input, event| {
                    if event == crate::ui::PromptEvent::Validate {
                        deliver_plugin_ui_callback(
                            cx,
                            &callback,
                            DynamicValue::String(input.to_string()),
                        );
                    } else if event == crate::ui::PromptEvent::Abort {
                        deliver_plugin_ui_callback(cx, &callback, DynamicValue::Nil);
                    }
                },
            );
            let prompt = if let Some(default) = request.default {
                prompt.with_line(default, editor)
            } else {
                prompt
            };
            compositor.push(Box::new(prompt));
        }
        PluginCommand::Confirm { request, callback } => {
            let prompt = crate::ui::Prompt::new(
                format!("{} (y/n) ", request.message).into(),
                None,
                crate::ui::completers::none,
                move |cx, input, event| {
                    if event == crate::ui::PromptEvent::Validate {
                        let confirmed =
                            input.to_lowercase() == "y" || input.to_lowercase() == "yes";
                        deliver_plugin_ui_callback(cx, &callback, DynamicValue::Bool(confirmed));
                    } else if event == crate::ui::PromptEvent::Abort {
                        deliver_plugin_ui_callback(cx, &callback, DynamicValue::Bool(false));
                    }
                },
            );
            compositor.push(Box::new(prompt));
        }
        PluginCommand::Picker { request, callback } => {
            let columns = [crate::ui::PickerColumn::new(
                "item",
                |item: &String, _data| item.as_str().into(),
            )];
            let picker = crate::ui::Picker::new(
                columns,
                0,
                request.items,
                (),
                crate::ui::PickerRuntime::new(editor),
                ingress,
                move |cx: &mut crate::compositor::Context, item: &String, _action| {
                    deliver_plugin_ui_callback(cx, &callback, DynamicValue::String(item.clone()));
                },
            );
            compositor.push(Box::new(crate::ui::overlay::overlaid(picker)));
        }
        PluginCommand::PushPanel {
            panel,
            content,
            key_events,
        } => {
            if let Some(component) =
                RoutedPluginPanel::from_editor(editor, panel, content, key_events)
            {
                compositor.push(Box::new(component));
            }
        }
        PluginCommand::RemovePanel { panel } => {
            let target_id = crate::ui::plugin_panel::component_id(panel);
            compositor.remove_by_id(&target_id);
        }
        PluginCommand::ReleaseResources { plugin, panels } => {
            for panel in panels {
                if let Ok(panel_id) = adapt::resolve_panel(&editor.model, panel) {
                    let _ = editor.model.remove_panel(panel_id);
                }
                compositor.remove_by_id(&crate::ui::plugin_panel::component_id(panel));
            }
            editor
                .model
                .remove_floats_by_owner(&plugin.raw().get().to_string());
        }
        PluginCommand::UpdatePanel {
            panel,
            title,
            content,
        } => {
            let Some(panel_id) = plugin_panel_id(editor, panel) else {
                return;
            };
            if let Some(panel) = editor.model.panels.get_mut(panel_id) {
                if let Some(title) = title {
                    panel.title = title;
                }
            }
            if let Some(content) = content {
                if let Some(component) = compositor
                    .find_id::<RoutedPluginPanel>(&crate::ui::plugin_panel::component_id(panel))
                {
                    component.set_content(content);
                }
            }
        }
        PluginCommand::TogglePanel { panel } => {
            let Some(panel_id) = plugin_panel_id(editor, panel) else {
                return;
            };
            let _ = editor.model.toggle_panel(panel_id);
        }
        PluginCommand::FocusPanel { panel } => {
            let Some(panel_id) = plugin_panel_id(editor, panel) else {
                return;
            };
            editor.model.focus_panel(panel_id);
        }
        PluginCommand::ResizePanel { panel, size } => {
            let Some(panel_id) = plugin_panel_id(editor, panel) else {
                return;
            };
            if let Some(panel) = editor.model.panels.get_mut(panel_id) {
                panel.size = match size {
                    PanelSizeSpec::Fixed(cells) => PanelSize::fixed(cells),
                    PanelSizeSpec::Percent(percent) => PanelSize::Percent(percent),
                };
            }
        }
    }
}
