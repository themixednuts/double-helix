use crate::{
    compositor::Compositor,
    runtime::{ui::command::PluginCommand, RuntimeEvent, RuntimeTaskEvent},
};
use helix_plugin::PluginManager;
use helix_runtime::Sender as IngressSender;

pub(crate) fn apply_plugin_command(
    editor: &mut helix_view::Editor,
    compositor: &mut Compositor,
    ingress: IngressSender<RuntimeEvent>,
    _plugin_manager: std::sync::Arc<PluginManager>,
    cmd: PluginCommand,
) {
    match cmd {
        PluginCommand::Prompt {
            message,
            default,
            plugin_name,
            callback_id,
        } => {
            let prompt = crate::ui::Prompt::new(
                message.into(),
                None,
                |_editor, _input| Vec::new(),
                move |cx, input, event| {
                    if event == crate::ui::PromptEvent::Validate {
                        helix_runtime::send_blocking(
                            &cx.ingress,
                            RuntimeEvent::Task(RuntimeTaskEvent::DeliverPluginUiCallback {
                                plugin_name: plugin_name.clone(),
                                callback_id,
                                value: serde_json::Value::String(input.to_string()),
                            }),
                        );
                    }
                },
            );
            let prompt = if let Some(default) = default {
                prompt.with_line(default, editor)
            } else {
                prompt
            };
            compositor.push(Box::new(prompt));
        }
        PluginCommand::Confirm {
            message,
            plugin_name,
            callback_id,
        } => {
            let prompt = crate::ui::Prompt::new(
                format!("{} (y/n) ", message).into(),
                None,
                |_editor, _input| Vec::new(),
                move |cx, input, event| {
                    if event == crate::ui::PromptEvent::Validate {
                        let confirmed =
                            input.to_lowercase() == "y" || input.to_lowercase() == "yes";
                        helix_runtime::send_blocking(
                            &cx.ingress,
                            RuntimeEvent::Task(RuntimeTaskEvent::DeliverPluginUiCallback {
                                plugin_name: plugin_name.clone(),
                                callback_id,
                                value: serde_json::Value::Bool(confirmed),
                            }),
                        );
                    } else if event == crate::ui::PromptEvent::Abort {
                        helix_runtime::send_blocking(
                            &cx.ingress,
                            RuntimeEvent::Task(RuntimeTaskEvent::DeliverPluginUiCallback {
                                plugin_name: plugin_name.clone(),
                                callback_id,
                                value: serde_json::Value::Bool(false),
                            }),
                        );
                    }
                },
            );
            compositor.push(Box::new(prompt));
        }
        PluginCommand::Picker {
            items,
            prompt: _,
            plugin_name,
            callback_id,
        } => {
            let columns = [crate::ui::PickerColumn::new(
                "item",
                |item: &String, _data| item.as_str().into(),
            )];
            let picker = crate::ui::Picker::new(
                columns,
                0,
                items,
                (),
                editor.runtime().clone(),
                ingress,
                move |cx: &mut crate::compositor::Context, item: &String, _action| {
                    helix_runtime::send_blocking(
                        &cx.ingress,
                        RuntimeEvent::Task(RuntimeTaskEvent::DeliverPluginUiCallback {
                            plugin_name: plugin_name.clone(),
                            callback_id,
                            value: serde_json::Value::String(item.clone()),
                        }),
                    );
                },
            );
            compositor.push(Box::new(crate::ui::overlay::overlaid(picker)));
        }
        PluginCommand::PushPanel {
            plugin_name,
            panel_id,
            model_panel_id,
            render_callback_id,
            event_callback_id,
        } => {
            let mut panel = crate::ui::plugin_panel::PluginPanel::new(
                plugin_name,
                panel_id,
                render_callback_id,
                event_callback_id,
            );
            panel.set_model_panel_id(model_panel_id);
            compositor.push(Box::new(panel));
        }
        PluginCommand::RemovePanel { panel_id } => {
            let target_id = format!("plugin_panel:{panel_id}");
            compositor.remove_by_id(&target_id);
        }
    }
}
