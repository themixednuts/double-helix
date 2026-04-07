use helix_plugin::PluginManager;
use helix_runtime::{send_blocking, Sender as IngressSender};

use crate::runtime::{ingress::RuntimeEvent, UiCommand};
use helix_view::Editor;

pub(crate) fn apply_register_plugin_panel(
    editor: &mut Editor,
    ingress: IngressSender<RuntimeEvent>,
    plugin_name: String,
    panel_id: String,
    title: String,
    side: String,
    width: u16,
    render_callback_id: u64,
    event_callback_id: Option<u64>,
) {
    use helix_view::model::{PanelSide, PanelSize, PluginPanelModel};

    let panel_side = match side.as_str() {
        "left" => PanelSide::Left,
        "bottom" => PanelSide::Bottom,
        _ => PanelSide::Right,
    };
    let model = PluginPanelModel {
        plugin_name: plugin_name.clone(),
        panel_id: panel_id.clone(),
        render_callback_id,
        event_callback_id,
    };
    let model_panel_id =
        editor
            .model
            .insert_panel(title, Box::new(model), panel_side, PanelSize::fixed(width));

    send_blocking(
        &ingress,
        RuntimeEvent::Ui(UiCommand::Plugin(
            crate::runtime::ui::command::PluginCommand::PushPanel {
                plugin_name,
                panel_id,
                model_panel_id,
                render_callback_id,
                event_callback_id,
            },
        )),
    );
}

pub(crate) fn apply_remove_plugin_panel(
    editor: &mut Editor,
    ingress: IngressSender<RuntimeEvent>,
    panel_id: String,
) {
    let matching: Vec<_> = editor
        .model
        .panels
        .iter()
        .filter_map(|(id, entry)| {
            entry
                .content
                .as_any()
                .downcast_ref::<helix_view::model::PluginPanelModel>()
                .filter(|model| model.panel_id == panel_id)
                .map(|_| id)
        })
        .collect();

    for id in matching {
        editor.model.remove_panel(id);
    }

    send_blocking(
        &ingress,
        RuntimeEvent::Ui(UiCommand::Plugin(
            crate::runtime::ui::command::PluginCommand::RemovePanel { panel_id },
        )),
    );
}

pub(crate) fn apply_plugin_ui_callback(
    editor: &mut Editor,
    plugin_manager: std::sync::Arc<PluginManager>,
    plugin_name: String,
    callback_id: u64,
    value: serde_json::Value,
) {
    if let Err(err) = plugin_manager.handle_ui_callback(editor, plugin_name, callback_id, value) {
        editor.set_error(err.to_string());
    }
}
