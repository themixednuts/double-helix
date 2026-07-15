#[derive(Debug, Clone)]
pub enum PluginCommand {
    SetTheme {
        theme: helix_view::Theme,
        completion: crate::plugin_registry::PluginTaskResponder,
    },
    RunCommand {
        request: helix_plugin_api::requests::RunCommandRequest,
        completion: crate::plugin_registry::PluginTaskResponder,
    },
    SetKeymap {
        keymap: helix_plugin_api::KeymapHandle,
        contribution: crate::keymap::CompiledKeymapContribution,
    },
    RemoveKeymap {
        keymap: helix_plugin_api::KeymapHandle,
    },
    Notify {
        level: helix_plugin_api::requests::NotifyLevel,
        message: String,
    },
    Prompt {
        request: helix_plugin_api::requests::PromptRequest,
        callback: crate::plugin_registry::PluginUiCallback,
    },
    Confirm {
        request: helix_plugin_api::requests::ConfirmRequest,
        callback: crate::plugin_registry::PluginUiCallback,
    },
    Picker {
        request: helix_plugin_api::requests::PickerRequest,
        callback: crate::plugin_registry::PluginUiCallback,
    },
    PushPanel {
        panel: helix_plugin_api::PanelHandle,
        content: std::sync::Arc<[helix_plugin_api::requests::UiRenderNode]>,
        key_events: Option<crate::plugin_registry::PluginPanelKeyRoute>,
    },
    RemovePanel {
        panel: helix_plugin_api::PanelHandle,
    },
    ReleaseResources {
        plugin: helix_plugin_api::PluginId,
        panels: Vec<helix_plugin_api::PanelHandle>,
    },
    UpdatePanel {
        panel: helix_plugin_api::PanelHandle,
        title: Option<String>,
        content: Option<std::sync::Arc<[helix_plugin_api::requests::UiRenderNode]>>,
    },
    TogglePanel {
        panel: helix_plugin_api::PanelHandle,
    },
    FocusPanel {
        panel: helix_plugin_api::PanelHandle,
    },
    ResizePanel {
        panel: helix_plugin_api::PanelHandle,
        size: helix_plugin_api::requests::PanelSizeSpec,
    },
}
