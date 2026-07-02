use helix_stdx::path::get_relative_path;
use tokio::time::Instant;
use tui::backend::Backend;

use helix_view::document::DocumentSavedEventResult;

use super::Application;

impl Application {
    pub(super) async fn service_idle_timeout(&mut self, render: crate::runtime::IdleRender) {
        let ingress = self.ingress().tx.clone();
        let idle_reset = self.ingress().idle_reset.clone();
        let redraw = self.editor.redraw_handle();
        let notifier = crate::handlers::local::Notifier {
            redraw: redraw.clone(),
            plugin_events: self.ingress().plugin_event_tx.clone(),
        };
        let mut cx = Self::make_compositor_context(
            &mut self.editor,
            &mut self.exit.tasks,
            self.exit.work.clone(),
            notifier,
            ingress,
            idle_reset,
            self.plugin_manager.clone(),
        );
        let should_render = self
            .compositor
            .handle_event(&super::Event::IdleTimeout, &mut cx);
        let syntax_refreshed = self.editor.refresh_one_stale_syntax();
        if self.editor.has_stale_syntax() {
            let timeout = self.editor.config().idle_timeout;
            self.timers.idle.as_mut().reset(Instant::now() + timeout);
        }
        if syntax_refreshed || self.editor.has_stale_syntax() {
            helix_view::bench::log_run_event("bench_idle_service", || {
                format!(
                    "syntax_refreshed={} stale_remaining={} render_immediately={} needs_redraw={}",
                    syntax_refreshed,
                    self.editor.has_stale_syntax(),
                    render.should_render_immediately(),
                    self.editor.is_redraw_pending()
                )
            });
        }
        if render.should_render_immediately()
            && (should_render || syntax_refreshed || self.editor.is_redraw_pending())
        {
            self.render().await;
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
            use helix_plugin::contract::{adapt, events};
            let event = events::PluginEvent::DocumentSaved(events::DocumentSavedEvent {
                document: adapt::document_handle(report.doc_id),
                path: Some(report.path.to_string_lossy().into_owned()),
                success: true,
            });
            if let Err(err) = self.plugin_manager.fire_event(&mut self.editor, &event) {
                log::error!("Failed to fire plugin event: {}", err);
            }
        }
    }

    pub(crate) async fn handle_assistant_update(
        &mut self,
        update: helix_view::assistant::backend::Update,
    ) {
        // Extract plugin events from the update before consuming it.
        let plugin_events = assistant_update_plugin_events(&update);

        let outcome = self.editor.apply_assistant_update(update);
        if let Some((thread, request)) = outcome.permission_request {
            let ingress = self.ingress().tx.clone();
            crate::runtime::ui::assistant::apply_assistant_command(
                &mut self.editor,
                &mut self.compositor,
                ingress,
                crate::runtime::AssistantCommand::ShowPermissionRequest { thread, request },
            );
        }
        self.editor.apply_assistant_effects(outcome.effects);

        // Dispatch assistant plugin events after state is settled.
        for event in plugin_events {
            if let Err(err) = self.plugin_manager.fire_event(&mut self.editor, &event) {
                log::error!("Failed to fire assistant plugin event: {err}");
            }
        }
    }

    pub fn restore_term(&mut self) -> std::io::Result<()> {
        use helix_view::graphics::CursorKind;
        self.terminal
            .backend_mut()
            .show_cursor(CursorKind::Block)
            .ok();
        self.terminal.restore()
    }

    pub async fn close(&mut self) -> Vec<anyhow::Error> {
        let mut errs = Vec::new();
        let ingress = self.ingress().tx.clone();
        errs.extend(
            crate::runtime::drain_exit_tasks_collect(
                &mut self.editor,
                &mut self.exit.tasks,
                ingress,
                self.plugin_manager.clone(),
            )
            .await,
        );

        if let Err(err) = self.editor.flush_writes().await {
            log::error!("Error writing: {}", err);
            errs.push(err);
        }

        errs.extend(self.editor.flush_assistant_persistence().await);

        if self.editor.close_language_servers(None).await.is_err() {
            log::error!("Timed out waiting for language servers to shutdown");
            errs.push(anyhow::format_err!(
                "Timed out waiting for language servers to shutdown"
            ));
        }

        errs
    }
}

/// Extract plugin-visible events from an assistant backend update.
///
/// Inspects the update variant to determine which contract events to emit.
/// Called before the update is consumed by `apply_assistant_update`.
fn assistant_update_plugin_events(
    update: &helix_view::assistant::backend::Update,
) -> Vec<helix_plugin::contract::events::PluginEvent> {
    use helix_plugin::contract::{adapt, events};
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
