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

    pub fn assistant_history_page(
        &self,
        scope: &crate::assistant::thread::Scope,
    ) -> Option<crate::assistant::history::Page> {
        self.assistant.history(scope).cloned()
    }

    pub fn active_assistant_backend_id(&self) -> Option<crate::assistant::backend::Id> {
        self.assistant.active_backend_id()
    }

    pub fn active_assistant_caps(&self) -> Option<&helix_acp::AgentCaps> {
        self.assistant
            .active_thread()
            .and_then(|(_, thread)| thread.caps())
    }

    pub fn assistant_known_sessions(&self) -> Vec<(String, crate::assistant::thread::Id)> {
        self.assistant
            .threads()
            .filter_map(|thread| match thread.origin() {
                crate::assistant::thread::Origin::Backend { remote, .. } => {
                    Some((remote.to_string(), thread.id))
                }
                crate::assistant::thread::Origin::Local => None,
            })
            .collect()
    }

    pub fn selected_assistant_subagent(
        &self,
    ) -> Option<crate::assistant::tool::SubagentSessionInfo> {
        self.assistant.selected_subagent()
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
        Ok(self.new_assistant_thread(backend, thread.clone_scope(), None))
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

    pub fn fork_submit_active_assistant_prompt(
        &mut self,
        entry: crate::assistant::thread::EntryId,
        text: String,
    ) -> anyhow::Result<Vec<crate::assistant::effect::Effect>> {
        let thread = self
            .assistant
            .active_id()
            .zip(self.assistant.active_backend_id())
            .map(|(thread, _)| thread)
            .context("Active assistant thread is not bound to a backend")?;
        Ok(self.fork_submit_assistant_prompt(thread, entry, text))
    }

    pub fn retry_active_assistant_prompt(
        &mut self,
    ) -> anyhow::Result<Vec<crate::assistant::effect::Effect>> {
        let (thread, text) = self
            .assistant
            .active_thread()
            .and_then(|(thread, state)| {
                crate::assistant::retry_prompt(state).map(|text| (thread, text))
            })
            .ok_or_else(|| anyhow::anyhow!("No failed assistant prompt to retry"))?;
        Ok(self.submit_assistant_prompt(thread, text))
    }

    pub fn delete_assistant_history_thread(
        &mut self,
        thread: crate::assistant::thread::Id,
        delete_remote: bool,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::DeleteHistoryThread {
            thread,
            delete_remote,
        })
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
        Option<String>,
    )> {
        let (id, thread) = self.assistant.active_thread()?;
        Some((
            id,
            thread.mode().cloned(),
            thread.config().clone(),
            thread.profile_name().map(ToOwned::to_owned),
        ))
    }

    pub fn active_assistant_note(&self) -> Option<String> {
        self.assistant
            .active_thread()
            .and_then(|(_, thread)| thread.feedback().note.clone())
    }

    pub fn set_active_assistant_profile(
        &mut self,
        profile: crate::assistant::profile::Defaults,
    ) -> anyhow::Result<Vec<crate::assistant::effect::Effect>> {
        let thread = self
            .assistant
            .active_id()
            .context("No active assistant thread")?;
        Ok(self.set_assistant_profile(thread, profile))
    }

    pub fn set_active_assistant_rating(
        &mut self,
        rating: crate::assistant::thread::Rating,
    ) -> anyhow::Result<Vec<crate::assistant::effect::Effect>> {
        let thread = self
            .assistant
            .active_id()
            .context("No active assistant thread")?;
        Ok(self.set_assistant_rating(thread, rating))
    }

    pub fn set_active_assistant_note(
        &mut self,
        note: Option<String>,
    ) -> anyhow::Result<Vec<crate::assistant::effect::Effect>> {
        let thread = self
            .assistant
            .active_id()
            .context("No active assistant thread")?;
        Ok(self.set_assistant_note(thread, note))
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
