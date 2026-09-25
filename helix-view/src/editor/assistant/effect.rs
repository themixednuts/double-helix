use super::Editor;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AcceptedReviewApplyDecision {
    ApplyAndSave,
    ApplyOnly,
    FsFallback,
}

pub(crate) fn accepted_review_apply_decision(
    document_open: bool,
    document_modified: bool,
) -> AcceptedReviewApplyDecision {
    match (document_open, document_modified) {
        (false, _) => AcceptedReviewApplyDecision::FsFallback,
        (true, false) => AcceptedReviewApplyDecision::ApplyAndSave,
        (true, true) => AcceptedReviewApplyDecision::ApplyOnly,
    }
}

impl Editor {
    /// The `LoadThread` that re-binds `command`'s thread to its agent session, for commands
    /// that need a live session.
    fn assistant_rebind_command(
        &self,
        command: &crate::assistant::backend::Command,
    ) -> Option<crate::assistant::backend::Command> {
        use crate::assistant::backend::Command;
        let (Command::Submit { thread, .. }
        | Command::ForkSubmit { thread, .. }
        | Command::SetMode { thread, .. }
        | Command::SetConfig { thread, .. }) = command
        else {
            return None;
        };
        let crate::assistant::thread::Origin::Backend { remote, .. } =
            self.assistant.thread(*thread)?.origin()
        else {
            return None;
        };
        Some(Command::LoadThread {
            thread: *thread,
            remote: remote.clone(),
        })
    }

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
                    self.request_location_reveal(
                        &location,
                        crate::handlers::NavigationPurpose::AssistantFollow,
                    );
                }
                crate::assistant::effect::Effect::SendBackendCommand { backend, command } => {
                    let was_live = self.take_live_assistant_backend(&backend).is_some();
                    let Some(handle) = self.ensure_assistant_backend(&backend) else {
                        self.set_error(format!("Assistant backend missing: {backend}"));
                        continue;
                    };
                    // A freshly started agent knows none of the sessions an earlier process had
                    // (after a crash or restart). Load the thread's session first, so the
                    // conversation continues instead of failing as unbound.
                    let rebind = if was_live {
                        None
                    } else {
                        self.assistant_rebind_command(&command)
                    };
                    self.runtime
                        .work()
                        .spawn(async move {
                            if let Some(rebind) = rebind {
                                let _ = handle.send(rebind).await;
                            }
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
                crate::assistant::effect::Effect::ApplyReviewAcceptedFile {
                    thread,
                    path,
                    text,
                } => {
                    self.apply_assistant_review_accepted_file(thread, path, text);
                }
                crate::assistant::effect::Effect::SetStatus { message } => {
                    self.set_status(message);
                }
                crate::assistant::effect::Effect::LaunchAuthTerminal {
                    thread,
                    title,
                    terminal,
                } => {
                    if let Err(error) = self.launch_auth_terminal(&terminal) {
                        let message = format!(
                            "{error}. Run `{}` in a terminal to sign in with {title}, then press enter.",
                            terminal.command_line()
                        );
                        if let Some(state) = self.assistant.thread_mut(thread) {
                            state.auth_mut().terminal_login_failed(message);
                        }
                        self.request_redraw();
                    }
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
                    // Every streamed chunk syncs the model, but the layout (which threads are
                    // open, which is active) rarely changes: save it only when it did.
                    let key = self.assistant_layout_key();
                    if self.assistant_persistence.layout_key != Some(key) {
                        self.assistant_persistence.layout_key = Some(key);
                        let scope = crate::assistant::layout::current_scope();
                        let (open, active) = self.assistant_layout_threads(&scope);
                        self.debounce_assistant_layout(async move {
                            let _ =
                                crate::assistant::layout::save_layout(&scope, open, active).await;
                        });
                    }
                    self.request_redraw();
                }
            }
        }
    }

    /// Open the login command in the external terminal (`editor.terminal`, as debug adapters
    /// use). It runs on its own; the user says when it finished.
    fn launch_auth_terminal(
        &self,
        terminal: &crate::assistant::auth::Terminal,
    ) -> Result<(), String> {
        let config =
            self.config().terminal.clone().ok_or_else(|| {
                "No external terminal is configured (`editor.terminal`)".to_string()
            })?;
        let mut child = std::process::Command::new(&config.command)
            .args(&config.args)
            .arg(&terminal.command)
            .args(&terminal.args)
            .envs(terminal.env.iter().map(|(key, value)| (key, value)))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|err| format!("Could not open a terminal with `{}`: {err}", config.command))?;
        // Reap it whenever it exits.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }

    fn apply_assistant_review_accepted_file(
        &mut self,
        _thread: crate::assistant::thread::Id,
        path: std::path::PathBuf,
        text: String,
    ) {
        let Some(doc_id) = self.document_id_by_review_path(&path) else {
            self.write_accepted_review_file(path, text);
            return;
        };

        let was_modified = self
            .document(doc_id)
            .map(|doc| doc.is_modified())
            .unwrap_or(false);
        let decision = accepted_review_apply_decision(true, was_modified);
        let view_id = self.get_synced_view_id(doc_id);
        let accepted = helix_core::Rope::from(text.as_str());
        let applied = self.with_view_doc_mut(view_id, doc_id, |view, doc| {
            let transaction = helix_core::diff::compare_ropes(doc.text(), &accepted);
            if transaction.changes().is_empty() {
                return false;
            }
            let applied = doc.apply(&transaction, view_id);
            if applied {
                doc.append_changes_to_history(view);
            }
            applied
        });

        match decision {
            AcceptedReviewApplyDecision::ApplyAndSave => {
                if applied {
                    if let Err(err) = self.save(doc_id, None, crate::editor::SavePolicy::Safe) {
                        self.set_error(format!("Accepted review edit but save failed: {err}"));
                        return;
                    }
                }
                self.set_status(format!("Accepted review edit: {}", path.display()));
            }
            AcceptedReviewApplyDecision::ApplyOnly => {
                self.set_status(format!(
                    "Accepted review edit in dirty buffer; not saved: {}",
                    path.display()
                ));
            }
            AcceptedReviewApplyDecision::FsFallback => unreachable!("open document was found"),
        }
    }

    fn document_id_by_review_path(&self, path: &std::path::Path) -> Option<crate::DocumentId> {
        let target = helix_stdx::path::canonicalize(path);
        self.documents().find_map(|doc| {
            let doc_path = doc.path()?;
            (helix_stdx::path::canonicalize(doc_path) == target).then_some(doc.id())
        })
    }

    fn write_accepted_review_file(&mut self, path: std::path::PathBuf, text: String) {
        let display = path.display().to_string();
        let error_display = display.clone();
        self.work()
            .spawn(async move {
                let result = async {
                    if let Some(parent) = path.parent() {
                        tokio::fs::create_dir_all(parent).await?;
                    }
                    tokio::fs::write(&path, text).await
                }
                .await;

                if let Err(err) = result {
                    log::warn!("failed to write accepted review edit {error_display}: {err}");
                }
            })
            .detach();
        self.set_status(format!("Accepted review edit: {display}"));
    }
}

#[cfg(test)]
mod tests {
    use super::{accepted_review_apply_decision, AcceptedReviewApplyDecision};

    #[test]
    fn accepted_review_apply_decision_saves_clean_open_documents() {
        assert_eq!(
            accepted_review_apply_decision(true, false),
            AcceptedReviewApplyDecision::ApplyAndSave
        );
    }

    #[test]
    fn accepted_review_apply_decision_leaves_dirty_open_documents_unsaved() {
        assert_eq!(
            accepted_review_apply_decision(true, true),
            AcceptedReviewApplyDecision::ApplyOnly
        );
    }

    #[test]
    fn accepted_review_apply_decision_falls_back_for_closed_documents() {
        assert_eq!(
            accepted_review_apply_decision(false, false),
            AcceptedReviewApplyDecision::FsFallback
        );
    }
}
