use helix_stdx::path::get_relative_path;
use helix_view::document::DocumentSavedEventResult;

use super::Application;

impl Application {
    pub(super) async fn service_idle_timeout(&mut self, render: crate::runtime::IdleRender) {
        let ingress = self.ingress().tx.clone();
        let idle_reset = self.ingress().idle_reset.clone();
        let redraw = self.editor.redraw_handle();
        let notifier = crate::handlers::local::Notifier {
            redraw: redraw.clone(),
            plugin_events: self.ingress().tx.clone().into(),
        };
        let mut cx = Self::make_compositor_context(
            &mut self.editor,
            &mut self.exit.tasks,
            self.exit.work.clone(),
            notifier,
            ingress,
            idle_reset,
            self.plugin_runtime.clone(),
            self.foreground.clone(),
        );
        let should_render = self
            .compositor
            .handle_event(&super::Event::IdleTimeout, &mut cx);
        drop(cx);
        self.drain_foreground();
        if render.should_render_immediately() && (should_render || self.editor.is_redraw_pending())
        {
            self.invalidate(super::FRAME_TIMER);
        }
    }

    pub async fn handle_idle_timeout(&mut self) {
        self.service_idle_timeout(crate::runtime::IdleRender::Immediate)
            .await;
    }

    pub fn handle_document_write(&mut self, doc_save_event: DocumentSavedEventResult) {
        let doc_save_event = match doc_save_event {
            Ok(Some(event)) => event,
            Ok(None) => return,
            Err(err) => {
                self.editor.set_error(err.to_string());
                return;
            }
        };

        let Some(report) = self.editor.apply_document_saved_event(doc_save_event) else {
            return;
        };

        let lines = report.line_count;
        let size = format_written_size(report.byte_count);

        self.editor.set_status(format!(
            "'{}' written, {lines}L {size}",
            get_relative_path(&report.path).to_string_lossy(),
        ));

        {
            use helix_plugin_api::events;
            use helix_plugin_editor::adapt;
            let event = events::PluginEvent::DocumentSaved(events::DocumentSavedEvent {
                document: adapt::document_handle(report.doc_id),
                path: Some(report.path.to_string_lossy().into_owned()),
                success: true,
            });
            self.plugin_runtime.notify_event(event);
        }
    }

    pub(crate) fn handle_assistant_update(
        &mut self,
        update: helix_view::assistant::backend::Update,
    ) {
        // Extract plugin events from the update before consuming it.
        let plugin_events = assistant_update_plugin_events(&update);
        let assistant_panel_focused = self
            .compositor
            .find_id::<crate::ui::assistant::AssistantPanel>(crate::ui::assistant::ID)
            .is_some_and(|panel| helix_view::traits::Focusable::is_focused(panel));
        let completion_toast = assistant_completion_toast_for_update(
            &self.editor,
            &update,
            self.editor.config().assistant.notify_on_done,
            assistant_panel_focused,
        );

        let outcome = self.editor.apply_assistant_update(update);
        if let Some((thread, request)) = outcome.permission_request {
            let ingress = self.ingress().tx.clone();
            crate::runtime::ui::assistant::apply_assistant_command(
                &mut self.editor,
                &mut self.compositor,
                ingress,
                self.foreground.clone(),
                crate::runtime::AssistantCommand::ShowPermissionRequest { thread, request },
            );
        }
        self.editor.apply_assistant_effects(outcome.effects);
        if let Some(toast) = completion_toast {
            match toast.severity {
                helix_view::editor::Severity::Error => {
                    self.editor.notify_error(toast.message);
                }
                helix_view::editor::Severity::Warning => {
                    self.editor.notify_warning(toast.message);
                }
                _ => {
                    self.editor.notify_info(toast.message);
                }
            }
        }

        // Dispatch assistant plugin events after state is settled.
        for event in plugin_events {
            self.plugin_runtime.notify_event(event);
        }
    }

    pub async fn restore_term(&mut self) -> std::io::Result<()> {
        if let Some(presenter) = &self.presenter {
            presenter.restore().await
        } else if let Some(terminal) = &mut self.terminal {
            terminal.restore()
        } else {
            Ok(())
        }
    }

    pub async fn close(&mut self) -> Vec<anyhow::Error> {
        let mut errs = Vec::new();
        let ingress = self.ingress().tx.clone();
        errs.extend(
            crate::runtime::drain_exit_tasks_collect(
                &mut self.editor,
                &mut self.exit.tasks,
                ingress,
                self.foreground.clone(),
                self.plugin_runtime.clone(),
            )
            .await,
        );

        if let Err(err) = self.editor.flush_writes().await {
            log::error!("Error writing: {}", err);
            errs.push(err);
        }

        errs.extend(self.editor.flush_assistant_persistence().await);

        if let Err(err) = self.restore_term().await {
            log::error!("Error restoring terminal: {}", err);
            errs.push(err.into());
        }
        self.editor.close_language_servers(None).await;

        errs
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AssistantCompletionToast {
    message: String,
    severity: helix_view::editor::Severity,
}

fn assistant_completion_toast(
    previous: Option<&helix_view::assistant::thread::Run>,
    next: &helix_view::assistant::thread::Run,
    notify_on_done: bool,
    panel_focused: bool,
) -> Option<AssistantCompletionToast> {
    use helix_view::assistant::thread::Run;

    if !notify_on_done || panel_focused {
        return None;
    }
    if !matches!(previous, Some(Run::Running | Run::Waiting)) {
        return None;
    }
    match next {
        Run::Idle => Some(AssistantCompletionToast {
            message: "Assistant run completed".to_string(),
            severity: helix_view::editor::Severity::Info,
        }),
        Run::Failed { message } => Some(AssistantCompletionToast {
            message: format!("Assistant run failed: {message}"),
            severity: helix_view::editor::Severity::Error,
        }),
        Run::Running | Run::Waiting => None,
    }
}

fn assistant_completion_toast_for_update(
    editor: &helix_view::Editor,
    update: &helix_view::assistant::backend::Update,
    notify_on_done: bool,
    panel_focused: bool,
) -> Option<AssistantCompletionToast> {
    let helix_view::assistant::backend::Update::Thread {
        thread,
        event: helix_view::assistant::thread::Event::Run(next),
    } = update
    else {
        return None;
    };
    let previous = editor.assistant.thread(*thread).map(|thread| thread.run());
    assistant_completion_toast(previous, next, notify_on_done, panel_focused)
}

/// Extract plugin-visible events from an assistant backend update.
///
/// Inspects the update variant to determine which contract events to emit.
/// Called before the update is consumed by `apply_assistant_update`.
fn assistant_update_plugin_events(
    update: &helix_view::assistant::backend::Update,
) -> Vec<helix_plugin_api::events::PluginEvent> {
    use helix_plugin_api::events;
    use helix_plugin_editor::adapt;
    use helix_view::assistant::{backend, thread};

    match update {
        backend::Update::Thread {
            thread,
            event: thread::Event::Run(run),
        } => {
            let thread = adapt::thread_handle(*thread);
            match run {
                thread::Run::Running => vec![events::PluginEvent::AssistantRunStarted(
                    events::AssistantRunStartedEvent { thread },
                )],
                thread::Run::Idle => vec![events::PluginEvent::AssistantRunCompleted(
                    events::AssistantRunCompletedEvent {
                        thread,
                        success: true,
                        error: None,
                    },
                )],
                thread::Run::Failed { message } => {
                    vec![events::PluginEvent::AssistantRunCompleted(
                        events::AssistantRunCompletedEvent {
                            thread,
                            success: false,
                            error: Some(message.clone()),
                        },
                    )]
                }
                thread::Run::Waiting => Vec::new(),
            }
        }
        backend::Update::Thread {
            thread,
            event: thread::Event::Content(thread::Content::Append(entry)),
        } => {
            let (kind, _) = adapt::entry_kind_to_contract(&entry.kind);
            vec![events::PluginEvent::AssistantMessageReceived(
                events::AssistantMessageReceivedEvent {
                    thread: adapt::thread_handle(*thread),
                    // Entry ID is assigned during apply, so we use 0 as placeholder.
                    // Plugins should use thread_entries() for full entry data.
                    entry_id: 0,
                    kind,
                },
            )]
        }
        _ => Vec::new(),
    }
}

fn format_written_size(size: usize) -> impl std::fmt::Display {
    enum Size {
        Bytes(u16),
        HumanReadable(f32, &'static str),
    }

    impl std::fmt::Display for Size {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Bytes(bytes) => write!(f, "{bytes}B"),
                Self::HumanReadable(size, suffix) => write!(f, "{size:.1}{suffix}"),
            }
        }
    }

    if size < 1024 {
        Size::Bytes(size as u16)
    } else {
        const SUFFIX: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
        let mut size = size as f32;
        let mut i = 0;
        while i < SUFFIX.len() - 1 && size >= 1024.0 {
            size /= 1024.0;
            i += 1;
        }
        Size::HumanReadable(size, SUFFIX[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_view::assistant::thread::Run;

    #[test]
    fn assistant_completion_toast_notifies_when_unfocused_run_completes() {
        assert_eq!(
            assistant_completion_toast(Some(&Run::Running), &Run::Idle, true, false),
            Some(AssistantCompletionToast {
                message: "Assistant run completed".to_string(),
                severity: helix_view::editor::Severity::Info,
            })
        );
    }

    #[test]
    fn assistant_completion_toast_respects_config_and_focus() {
        assert_eq!(
            assistant_completion_toast(Some(&Run::Running), &Run::Idle, false, false),
            None
        );
        assert_eq!(
            assistant_completion_toast(Some(&Run::Running), &Run::Idle, true, true),
            None
        );
    }

    #[test]
    fn assistant_completion_toast_reports_failed_runs() {
        assert_eq!(
            assistant_completion_toast(
                Some(&Run::Waiting),
                &Run::Failed {
                    message: "boom".to_string()
                },
                true,
                false
            ),
            Some(AssistantCompletionToast {
                message: "Assistant run failed: boom".to_string(),
                severity: helix_view::editor::Severity::Error,
            })
        );
    }

    #[test]
    fn assistant_completion_toast_ignores_non_terminal_transitions() {
        assert_eq!(
            assistant_completion_toast(Some(&Run::Idle), &Run::Idle, true, false),
            None
        );
        assert_eq!(
            assistant_completion_toast(Some(&Run::Running), &Run::Running, true, false),
            None
        );
    }
}
