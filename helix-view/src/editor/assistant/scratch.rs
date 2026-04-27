use super::{Action, Editor};

impl Editor {
    pub fn open_selected_assistant_turn_changes(&mut self) -> bool {
        let Some(summary) = self.assistant.selected_change_summary() else {
            return false;
        };
        self.open_markdown_scratch(Action::Replace, summary.to_markdown("Turn Changes"));
        true
    }

    pub fn open_active_assistant_thread_changes(&mut self) -> bool {
        let Some(summary) = self.assistant.active_change_summary() else {
            return false;
        };
        self.open_markdown_scratch(Action::Replace, summary.to_markdown("Thread Changes"));
        true
    }
}
