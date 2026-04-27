use super::Editor;
use crate::editor::AssistantUpdateOutcome;

impl Editor {
    pub fn assistant_act(
        &mut self,
        action: crate::assistant::Action,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant.act(action)
    }

    pub fn apply_assistant_thread_event(
        &mut self,
        thread: crate::assistant::thread::Id,
        event: crate::assistant::thread::Event,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant
            .apply(crate::assistant::event::Event::Thread { thread, event })
    }

    pub fn apply_assistant_backend_event(
        &mut self,
        backend: crate::assistant::backend::Id,
        event: crate::assistant::backend::Event,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant
            .apply(crate::assistant::event::Event::Backend { backend, event })
    }

    pub fn replace_assistant_history(
        &mut self,
        scope: crate::assistant::thread::Scope,
        entries: Vec<crate::assistant::history::Stub>,
        next: Option<crate::assistant::history::Cursor>,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant.replace_history(scope, entries, next)
    }

    pub fn apply_assistant_location_update(
        &mut self,
        thread: crate::assistant::thread::Id,
        location: crate::collab::Location,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.apply_assistant_thread_event(thread, crate::assistant::thread::Event::Follow(location))
    }

    pub fn apply_assistant_terminal_event(
        &mut self,
        thread: crate::assistant::thread::Id,
        event: crate::assistant::terminal::Event,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.apply_assistant_thread_event(thread, crate::assistant::thread::Event::Terminal(event))
    }

    pub fn apply_assistant_update(
        &mut self,
        update: crate::assistant::backend::Update,
    ) -> AssistantUpdateOutcome {
        use crate::assistant::backend;

        match update {
            backend::Update::Thread { thread, event } => AssistantUpdateOutcome {
                effects: self.apply_assistant_thread_event(thread, event),
                permission_request: None,
            },
            backend::Update::Permission { thread, request } => AssistantUpdateOutcome {
                effects: Vec::new(),
                permission_request: Some((thread, request)),
            },
            backend::Update::Backend { backend, event } => AssistantUpdateOutcome {
                effects: self.apply_assistant_backend_event(backend, event),
                permission_request: None,
            },
            backend::Update::History {
                scope,
                entries,
                next,
            } => AssistantUpdateOutcome {
                effects: self.replace_assistant_history(scope, entries, next),
                permission_request: None,
            },
            backend::Update::Location { thread, location } => AssistantUpdateOutcome {
                effects: self.apply_assistant_location_update(thread, location),
                permission_request: None,
            },
            backend::Update::Terminal { thread, event } => AssistantUpdateOutcome {
                effects: self.apply_assistant_terminal_event(thread, event),
                permission_request: None,
            },
            backend::Update::Error { at, error } => {
                match at {
                    backend::Target::Backend(_) => self.set_error(error.to_string()),
                    backend::Target::Thread(_) => self.set_status(error.to_string()),
                }
                AssistantUpdateOutcome {
                    effects: Vec::new(),
                    permission_request: None,
                }
            }
        }
    }
}
