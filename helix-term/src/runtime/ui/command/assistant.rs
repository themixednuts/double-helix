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
        entries: Vec<helix_view::assistant::history::Stub>,
    },
    /// Show a picker for detaching one of several attached assistant context items.
    PushDetachContextPicker {
        items: Vec<helix_view::assistant::context::Item>,
    },
    ShowPermissionRequest {
        thread: helix_view::assistant::thread::Id,
        request: helix_view::assistant::permission::Request,
    },
    /// Open the assistant panel shell; assistant data comes from editor-owned state.
    OpenPanel,
}
