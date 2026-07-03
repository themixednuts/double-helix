use helix_view::editor::AgentConfig;

/// Assistant panel UI ingress.
#[derive(Debug, Clone)]
pub enum AssistantCommand {
    TogglePanelFocus,
    ClosePanel,
    FocusPanelInput,
    FocusPanelEntries,
    /// `:assistant-connect` with no args - pick from configured `[[editor.agents]]`.
    PushConfiguredAgentsPicker {
        agents: Vec<AgentConfig>,
    },
    /// Show assistant history entries using normalized history stubs.
    PushHistoryPicker {
        scope: helix_view::assistant::thread::Scope,
        entries: Vec<helix_view::assistant::history::Stub>,
        next: Option<helix_view::assistant::history::Cursor>,
    },
    /// Show a picker for detaching one of several attached assistant context items.
    PushDetachContextPicker {
        items: Vec<helix_view::assistant::context::Item>,
    },
    PushModeConfigPicker {
        thread: helix_view::assistant::thread::Id,
        items: Vec<ModeConfigPickerItem>,
    },
    ShowPermissionRequest {
        thread: helix_view::assistant::thread::Id,
        request: helix_view::assistant::permission::Request,
    },
    /// Open the assistant panel shell; assistant data comes from editor-owned state.
    OpenPanel,
}

#[derive(Debug, Clone)]
pub enum ModeConfigPickerItem {
    Mode {
        id: helix_view::assistant::mode::Id,
        name: String,
        current: bool,
    },
    Config {
        option: helix_view::assistant::config::Id,
        value: helix_view::assistant::config::ValueId,
        name: String,
        value_label: String,
        category: Option<String>,
        current: bool,
    },
}
