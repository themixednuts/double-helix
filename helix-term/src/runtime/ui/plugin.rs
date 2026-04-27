use crate::{
    compositor::Compositor,
    runtime::{ui::command::PluginCommand, RuntimeEvent, RuntimeTaskEvent},
};
use helix_plugin::contract::DynamicValue;
use helix_plugin::contract::UiCallbackToken;
use helix_plugin::PluginManager;
use helix_runtime::Sender as IngressSender;

fn deliver_plugin_ui_callback(
    ingress: &IngressSender<RuntimeEvent>,
    callback: UiCallbackToken,
    value: DynamicValue,
) {
    helix_runtime::send_blocking(
        ingress,
        RuntimeEvent::Task(RuntimeTaskEvent::DeliverPluginUiCallback { callback, value }),
    );
}

pub(crate) fn apply_plugin_command(
    editor: &mut helix_view::Editor,
    compositor: &mut Compositor,
    ingress: IngressSender<RuntimeEvent>,
    _plugin_manager: std::sync::Arc<PluginManager>,
    cmd: PluginCommand,
) {
    match cmd {
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
                crate::ui::PickerRuntime::new(editor.runtime()),
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
    }
}
