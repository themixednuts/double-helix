#[derive(Debug, Clone)]
pub enum PluginCommand {
    Notify {
        level: helix_plugin::contract::requests::NotifyLevel,
        message: String,
    },
    Prompt {
        request: helix_plugin::contract::requests::PromptRequest,
        callback: helix_plugin::contract::UiCallbackToken,
    },
    Confirm {
        request: helix_plugin::contract::requests::ConfirmRequest,
        callback: helix_plugin::contract::UiCallbackToken,
    },
    Picker {
        request: helix_plugin::contract::requests::PickerRequest,
        callback: helix_plugin::contract::UiCallbackToken,
    },
    PushPanel {
        panel: helix_plugin::contract::PanelHandle,
    },
    RemovePanel {
        panel: helix_plugin::contract::PanelHandle,
    },
    UpdatePanel {
        panel: helix_plugin::contract::PanelHandle,
        title: Option<String>,
    },
    TogglePanel {
        panel: helix_plugin::contract::PanelHandle,
    },
    FocusPanel {
        panel: helix_plugin::contract::PanelHandle,
    },
    ResizePanel {
        panel: helix_plugin::contract::PanelHandle,
        size: helix_plugin::contract::requests::PanelSizeSpec,
    },
}
