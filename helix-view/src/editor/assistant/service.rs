use crate::Editor;

impl Editor {
    pub fn assistant_history_backend(&self) -> Option<crate::assistant::history::Backend> {
        self.assistant_services.history.clone()
    }

    pub fn set_assistant_history_backend(&mut self, history: crate::assistant::history::Backend) {
        self.assistant_services.history = Some(history);
    }

    pub fn set_assistant_context_registry(
        &mut self,
        registry: crate::assistant::context::Registry,
    ) {
        self.assistant_services.context = registry;
    }
}
