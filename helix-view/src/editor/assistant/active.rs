use anyhow::Context;

use super::Editor;

impl Editor {
    pub fn has_assistant_threads(&self) -> bool {
        self.assistant.has_threads()
    }

    pub fn assistant_thread_exists(&self, thread: crate::assistant::thread::Id) -> bool {
        self.assistant.thread(thread).is_some()
    }

    pub fn active_assistant_snapshot(&self) -> Option<crate::assistant::thread::Snapshot> {
        self.assistant.active_snapshot()
    }

    pub fn active_assistant_context(&self) -> Option<Vec<crate::assistant::context::Item>> {
        self.assistant.active_context()
    }

    pub fn active_assistant_scope_or_layout(&self) -> crate::assistant::thread::Scope {
        self.assistant.active_scope_or_layout()
    }

    pub fn assistant_history_entries(
        &self,
        scope: &crate::assistant::thread::Scope,
    ) -> Option<Vec<crate::assistant::history::Stub>> {
        self.assistant.history_entries(scope)
    }

    pub fn assistant_model(&self, focused: bool) -> crate::model::AssistantModel {
        self.assistant.assistant_model(focused)
    }

    pub fn assistant_entry_markdown(
        &self,
        focused: bool,
        entry: crate::assistant::thread::EntryId,
    ) -> Option<String> {
        self.assistant.panel(focused).entry_markdown(entry)
    }

    pub fn assistant_entry_id_at(
        &self,
        focused: bool,
        index: usize,
    ) -> Option<crate::assistant::thread::EntryId> {
        self.assistant.panel(focused).entry_id_at(index)
    }

    pub fn is_assistant_entry_folded(
        &self,
        focused: bool,
        entry: crate::assistant::thread::EntryId,
    ) -> bool {
        self.assistant.panel(focused).is_entry_folded(entry)
    }

    pub fn create_local_assistant_thread(
        &mut self,
        scope: crate::assistant::thread::Scope,
    ) -> crate::assistant::thread::Id {
        self.assistant
            .create(crate::assistant::thread::Origin::Local, scope)
    }

    pub fn new_assistant_thread_from_active_backend(
        &mut self,
    ) -> anyhow::Result<Vec<crate::assistant::effect::Effect>> {
        let (_, thread) = self
            .assistant
            .active_thread_owned()
            .context("No active assistant thread")?;
        let backend = thread
            .backend_id()
            .context("Active assistant thread is not backend-backed")?;
        Ok(self.new_assistant_thread(backend, thread.clone_scope()))
    }

    pub fn cancel_active_assistant_thread(
        &mut self,
    ) -> Option<Vec<crate::assistant::effect::Effect>> {
        Some(self.cancel_assistant_thread(self.assistant.active_id()?))
    }

    pub fn submit_active_assistant_prompt(
        &mut self,
        text: String,
    ) -> anyhow::Result<Vec<crate::assistant::effect::Effect>> {
        let thread = self
            .assistant
            .active_id()
            .zip(self.assistant.active_backend_id())
            .map(|(thread, _)| thread)
            .context("Active assistant thread is not bound to a backend")?;
        Ok(self.submit_assistant_prompt(thread, text))
    }

    pub fn set_active_assistant_draft_if_changed(
        &mut self,
        text: String,
    ) -> Option<Vec<crate::assistant::effect::Effect>> {
        let (thread, state) = self.assistant.active_thread_owned()?;
        if state.draft() == text {
            return None;
        }
        Some(self.set_assistant_draft(thread, text))
    }

    pub fn attach_active_assistant_context(
        &mut self,
        item: crate::assistant::context::Kind,
    ) -> anyhow::Result<Vec<crate::assistant::effect::Effect>> {
        let thread = self
            .assistant
            .active_id()
            .context("No active assistant thread")?;
        Ok(self.attach_assistant_context(thread, item))
    }

    pub fn detach_active_assistant_context(
        &mut self,
        item: crate::assistant::context::Id,
    ) -> anyhow::Result<Vec<crate::assistant::effect::Effect>> {
        let thread = self
            .assistant
            .active_id()
            .context("No active assistant thread")?;
        Ok(self.detach_assistant_context(thread, item))
    }

    pub fn cycle_active_assistant_config(
        &mut self,
        key: &str,
    ) -> anyhow::Result<Vec<crate::assistant::effect::Effect>> {
        let (thread, option, value) = self
            .assistant
            .active_cycle_config(key)
            .with_context(|| format!("No {key} options from assistant backend"))?;
        Ok(self.set_assistant_config(thread, option, value))
    }

    pub fn cycle_active_assistant_mode(
        &mut self,
    ) -> anyhow::Result<Vec<crate::assistant::effect::Effect>> {
        let (thread, next) = self
            .assistant
            .active_next_mode()
            .context("No mode options from assistant backend")?;
        Ok(self.set_assistant_mode(thread, next))
    }

    pub fn active_assistant_mode_config(
        &self,
    ) -> Option<(
        crate::assistant::thread::Id,
        Option<crate::assistant::mode::Set>,
        crate::assistant::config::State,
    )> {
        let (id, thread) = self.assistant.active_thread()?;
        Some((id, thread.mode().cloned(), thread.config().clone()))
    }

    pub fn cycle_active_assistant_thread(
        &mut self,
        delta: isize,
    ) -> anyhow::Result<Vec<crate::assistant::effect::Effect>> {
        let next = self
            .assistant
            .next_thread(delta)
            .context("Need at least two assistant threads")?;
        Ok(self.activate_assistant_thread(next))
    }

    pub fn set_active_assistant_focus(
        &mut self,
        focus: crate::assistant::thread::Focus,
    ) -> anyhow::Result<Vec<crate::assistant::effect::Effect>> {
        let thread = self
            .assistant
            .active_id()
            .context("No active assistant thread")?;
        Ok(self.focus_assistant_thread(thread, focus))
    }

    pub fn select_active_assistant_entry(
        &mut self,
        entry: Option<crate::assistant::thread::EntryId>,
    ) -> anyhow::Result<Vec<crate::assistant::effect::Effect>> {
        let thread = self
            .assistant
            .active_id()
            .context("No active assistant thread")?;
        Ok(self.select_assistant_entry(thread, entry))
    }

    pub fn toggle_active_assistant_review_mode(
        &mut self,
    ) -> anyhow::Result<(String, Vec<crate::assistant::effect::Effect>)> {
        let (thread, state) = self
            .assistant
            .active_thread_owned()
            .context("No active assistant thread")?;
        let mode = state.review_mode().toggled();
        Ok((
            format!("Assistant review mode: {}", mode.label()),
            self.set_assistant_review_mode(thread, mode),
        ))
    }

    pub fn resolve_selected_assistant_review(
        &mut self,
        decision: crate::assistant::review::Decision,
    ) -> anyhow::Result<Vec<crate::assistant::effect::Effect>> {
        let (thread, target) = self
            .assistant
            .selected_review_target()
            .context("No pending review file selected")?;
        Ok(self.resolve_assistant_review(thread, target, decision))
    }

    pub fn resolve_all_active_assistant_review(
        &mut self,
        decision: crate::assistant::review::Decision,
    ) -> anyhow::Result<Vec<crate::assistant::effect::Effect>> {
        let thread = self
            .assistant
            .active_id()
            .context("No active assistant thread")?;
        Ok(self.resolve_assistant_review(thread, crate::assistant::review::Target::All, decision))
    }
}
