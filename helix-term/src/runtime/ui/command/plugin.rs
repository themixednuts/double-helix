#[derive(Debug, Clone)]
pub enum PluginCommand {
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
}
