use std::{
    sync::{
        atomic::{self, AtomicBool},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::Ok;
use arc_swap::access::Access;

use crate::runtime::RuntimeTaskEvent;
use helix_runtime::Runtime;
use helix_view::bench::log_command_phase;
use helix_view::handlers::{AutoSaveEvent, Handlers};

#[derive(Debug)]
pub(super) struct AutoSaveHandler {
    save_pending: Arc<AtomicBool>,
    armed: bool,
    deadline: Option<Instant>,
    clock: helix_runtime::Clock,
    ingress: crate::runtime::RuntimeIngress,
}

impl AutoSaveHandler {
    fn new(
        clock: helix_runtime::Clock,
        ingress: crate::runtime::RuntimeIngress,
    ) -> AutoSaveHandler {
        AutoSaveHandler {
            save_pending: Default::default(),
            armed: false,
            deadline: None,
            clock,
            ingress,
        }
    }

    async fn event(&mut self, event: AutoSaveEvent) {
        match event {
            AutoSaveEvent::DocumentChanged { save_after } => {
                self.armed = true;
                self.deadline = Some(self.clock.deadline_after(Duration::from_millis(save_after)));
            }
            AutoSaveEvent::LeftInsertMode => {
                if !self.armed && self.save_pending.load(atomic::Ordering::Relaxed) {
                    self.request_save().await;
                }
            }
        }
    }

    async fn request_save(&self) {
        let _ = self
            .ingress
            .send_task(RuntimeTaskEvent::AutoSaveRun {
                save_pending: self.save_pending.clone(),
            })
            .await;
    }

    async fn run(mut self, mut rx: helix_runtime::Receiver<AutoSaveEvent>) {
        loop {
            if let Some(deadline) = self.deadline {
                let mut timer = self.clock.timer_at(deadline);
                tokio::select! {
                    biased;
                    event = rx.recv() => {
                        let Some(event) = event else { break };
                        self.event(event).await;
                    }
                    _ = &mut timer => {
                        self.deadline = None;
                        self.armed = false;
                        self.request_save().await;
                    }
                }
            } else {
                let Some(event) = rx.recv().await else { break };
                self.event(event).await;
            }
        }
    }

    pub fn spawn(
        runtime: Runtime,
        ingress: crate::runtime::RuntimeIngress,
    ) -> helix_runtime::Sender<AutoSaveEvent> {
        let (tx, rx) = helix_runtime::channel(128);
        let work = runtime.work().clone();
        let clock = runtime.clock().clone();
        work.clone()
            .spawn(async move {
                AutoSaveHandler::new(clock, ingress).run(rx).await;
            })
            .detach();
        tx
    }
}

pub(super) fn attach(editor: &helix_view::Editor, handlers: &Handlers) {
    let changes = handlers.auto_save.clone();
    editor.lifecycle().on_document_change(move |event| {
        let hook_start = std::time::Instant::now();
        let config = event.doc.config.load();
        if config.auto_save.after_delay.enable {
            changes.send(AutoSaveEvent::DocumentChanged {
                save_after: config.auto_save.after_delay.timeout,
            });
        }
        let hook_dur = hook_start.elapsed();
        log_command_phase("document_did_change_hook", "auto_save", hook_dur, || {
            format!(
                "doc_id={:?} enabled={} timeout_ms={} lines={} bytes={}",
                event.doc.id(),
                config.auto_save.after_delay.enable,
                config.auto_save.after_delay.timeout,
                event.doc.text().len_lines(),
                event.doc.text().len_bytes()
            )
        });
        Ok(())
    });
}
