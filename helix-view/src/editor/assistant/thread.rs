use super::Editor;
use crate::DocumentId;

impl Editor {
    pub fn untrack_assistant_doc(
        &mut self,
        doc: DocumentId,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::UntrackDoc { doc })
    }

    pub fn activate_assistant_thread(
        &mut self,
        thread: crate::assistant::thread::Id,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::Activate { thread })
    }

    pub fn focus_assistant_thread(
        &mut self,
        thread: crate::assistant::thread::Id,
        focus: crate::assistant::thread::Focus,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::Focus { thread, focus })
    }

    pub fn select_assistant_entry(
        &mut self,
        thread: crate::assistant::thread::Id,
        entry: Option<crate::assistant::thread::EntryId>,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::SelectEntry { thread, entry })
    }

    pub fn load_assistant_thread(
        &mut self,
        record: crate::assistant::history::Record,
        activation: crate::editor::Activation,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::LoadThread {
            record: Box::new(record),
            activation,
        })
    }

    pub fn close_assistant_thread(
        &mut self,
        thread: crate::assistant::thread::Id,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::Close { thread })
    }

    pub fn new_assistant_thread(
        &mut self,
        backend: crate::assistant::backend::Id,
        scope: crate::assistant::thread::Scope,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::NewThread { backend, scope })
    }

    pub fn cancel_assistant_thread(
        &mut self,
        thread: crate::assistant::thread::Id,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::Cancel { thread })
    }

    pub fn attach_assistant_context(
        &mut self,
        thread: crate::assistant::thread::Id,
        item: crate::assistant::context::Kind,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::AttachContext { thread, item })
    }

    pub fn detach_assistant_context(
        &mut self,
        thread: crate::assistant::thread::Id,
        item: crate::assistant::context::Id,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::DetachContext { thread, item })
    }

    pub fn set_assistant_config(
        &mut self,
        thread: crate::assistant::thread::Id,
        option: crate::assistant::config::Id,
        value: crate::assistant::config::ValueId,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::SetConfig {
            thread,
            option,
            value,
        })
    }

    pub fn set_assistant_mode(
        &mut self,
        thread: crate::assistant::thread::Id,
        mode: crate::assistant::mode::Id,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::SetMode { thread, mode })
    }

    pub fn set_assistant_draft(
        &mut self,
        thread: crate::assistant::thread::Id,
        text: String,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::SetDraft { thread, text })
    }

    pub fn resolve_assistant_permission(
        &mut self,
        thread: crate::assistant::thread::Id,
        request: crate::assistant::permission::RequestId,
        decision: crate::assistant::permission::Decision,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::ResolvePermission {
            thread,
            request,
            decision,
        })
    }

    pub fn complete_assistant_elicitation(
        &mut self,
        thread: crate::assistant::thread::Id,
        id: String,
        response: crate::assistant::thread::ElicitationResponse,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::CompleteElicitation {
            thread,
            id,
            response,
        })
    }

    pub fn authenticate_assistant(
        &mut self,
        thread: crate::assistant::thread::Id,
        method: String,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::Authenticate { thread, method })
    }

    pub fn submit_assistant_prompt(
        &mut self,
        thread: crate::assistant::thread::Id,
        text: String,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::Submit { thread, text })
    }

    pub fn set_assistant_review_mode(
        &mut self,
        thread: crate::assistant::thread::Id,
        mode: crate::assistant::review::Mode,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::SetReviewMode { thread, mode })
    }

    pub fn resolve_assistant_review(
        &mut self,
        thread: crate::assistant::thread::Id,
        target: crate::assistant::review::Target,
        decision: crate::assistant::review::Decision,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::ResolveReview {
            thread,
            target,
            decision,
        })
    }
}
