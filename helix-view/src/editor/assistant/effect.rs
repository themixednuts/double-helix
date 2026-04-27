use super::{Action, Editor};

impl Editor {
    pub fn apply_assistant_effects(&mut self, effects: Vec<crate::assistant::effect::Effect>) {
        for effect in effects {
            match effect {
                crate::assistant::effect::Effect::EnsureParticipant { thread } => {
                    self.ensure_assistant_participant(thread);
                }
                crate::assistant::effect::Effect::LeaveParticipant { thread } => {
                    let _ = self.leave_participant(crate::assistant::thread::participant(thread));
                }
                crate::assistant::effect::Effect::PublishLocation { thread, location } => {
                    self.ensure_assistant_participant(thread);
                    let participant = crate::assistant::thread::participant(thread);
                    let _ = self.publish_location(participant, location);
                }
                crate::assistant::effect::Effect::RevealLocation { location } => {
                    self.assistant_follow.suppress_pause = true;
                    let _ = self.reveal_location(&location, Action::Replace);
                }
                crate::assistant::effect::Effect::SendBackendCommand { backend, command } => {
                    let Some(handle) = self.ensure_assistant_backend(&backend) else {
                        self.set_error(format!("Assistant backend missing: {backend}"));
                        continue;
                    };
                    self.runtime
                        .work()
                        .spawn(async move {
                            let _ = handle.send(command).await;
                        })
                        .detach();
                }
                crate::assistant::effect::Effect::OpenEntryDoc {
                    thread,
                    entry,
                    action,
                } => {
                    if let Some(effects) = self.open_assistant_entry_scratch(thread, entry, action)
                    {
                        self.apply_assistant_effects(effects);
                    }
                }
                crate::assistant::effect::Effect::SetStatus { message } => {
                    self.set_status(message);
                }
                crate::assistant::effect::Effect::Save { thread } => {
                    self.save_assistant_thread(thread);
                }
                crate::assistant::effect::Effect::SaveNow { record } => {
                    self.save_assistant_record_now(*record);
                }
                crate::assistant::effect::Effect::Delete { thread } => {
                    self.delete_assistant_thread(thread);
                }
                crate::assistant::effect::Effect::SyncModel => {
                    let scope = crate::assistant::layout::current_scope();
                    let (open, active) = self.assistant_layout_threads(&scope);
                    self.debounce_assistant_layout(async move {
                        let _ = crate::assistant::layout::save_layout(&scope, open, active).await;
                    });
                    self.request_redraw();
                }
            }
        }
    }
}
