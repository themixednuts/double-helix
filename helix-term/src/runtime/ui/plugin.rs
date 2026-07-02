use crate::{
    compositor::Compositor,
    runtime::{ui::command::PluginCommand, RuntimeTaskEvent},
};
use helix_plugin::contract::requests::{NotifyLevel, PanelSizeSpec};
use helix_plugin::contract::UiCallbackToken;
use helix_plugin::contract::{adapt, DynamicValue, PanelHandle};
use helix_plugin::PluginManager;
use helix_view::model::PanelSize;

fn deliver_plugin_ui_callback(
    ingress: &crate::runtime::RuntimeIngress,
    callback: UiCallbackToken,
    value: DynamicValue,
) {
    ingress.task(RuntimeTaskEvent::DeliverPluginUiCallback { callback, value });
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
    editor: &mut helix_view::Editor,
    compositor: &mut Compositor,
    ingress: crate::runtime::RuntimeIngress,
    _plugin_manager: std::sync::Arc<PluginManager>,
    cmd: PluginCommand,
) {
    match cmd {
        PluginCommand::Notify { level, message } => match level {
            NotifyLevel::Info => editor.set_status(message),
            NotifyLevel::Warn => editor.set_status(format!("Warning: {message}")),
            NotifyLevel::Error => editor.set_error(message),
        },
        PluginCommand::Prompt { request, callback } => {
            let prompt = crate::ui::Prompt::new(
                request.message.into(),
                None,
                |_editor, _input| Vec::new(),
                move |cx, input, event| {
                    if event == crate::ui::PromptEvent::Validate {
                        deliver_plugin_ui_callback(
                            &cx.ingress,
                            callback,
                            DynamicValue::String(input.to_string()),
                        );
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
                |_editor, _input| Vec::new(),
                move |cx, input, event| {
                    if event == crate::ui::PromptEvent::Validate {
                        let confirmed =
                            input.to_lowercase() == "y" || input.to_lowercase() == "yes";
                        deliver_plugin_ui_callback(
                            &cx.ingress,
                            callback,
                            DynamicValue::Bool(confirmed),
                        );
                    } else if event == crate::ui::PromptEvent::Abort {
                        deliver_plugin_ui_callback(
                            &cx.ingress,
                            callback,
                            DynamicValue::Bool(false),
                        );
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
                    deliver_plugin_ui_callback(
                        &cx.ingress,
                        callback,
                        DynamicValue::String(item.clone()),
                    );
                },
            );
            compositor.push(Box::new(crate::ui::overlay::overlaid(picker)));
        }
        PluginCommand::PushPanel { panel } => {
            if let Some(component) =
                crate::ui::plugin_panel::PluginPanel::from_editor(editor, panel)
            {
                compositor.push(Box::new(component));
            }
        }
        PluginCommand::RemovePanel { panel } => {
            let target_id = crate::ui::plugin_panel::component_id(panel);
            compositor.remove_by_id(&target_id);
        }
        PluginCommand::UpdatePanel { panel, title } => {
            let Some(panel_id) = plugin_panel_id(editor, panel) else {
                return;
            };
            if let Some(panel) = editor.model.panels.get_mut(panel_id) {
                if let Some(title) = title {
                    panel.title = title;
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
