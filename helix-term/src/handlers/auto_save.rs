use std::{
    sync::{
        atomic::{self, AtomicBool},
        Arc,
    },
    time::Duration,
};

use anyhow::Ok;
use arc_swap::access::Access;

use crate::runtime::RuntimeTaskEvent;
use helix_runtime::{send_blocking, Runtime, Work};
use helix_view::bench::log_command_phase;
use helix_view::handlers::{AutoSaveEvent, Handlers};

#[derive(Debug)]
pub(super) struct AutoSaveHandler {
    save_pending: Arc<AtomicBool>,
    armed: Arc<AtomicBool>,
    debouncer: crate::runtime::RuntimeTaskDebouncer,
}

impl AutoSaveHandler {
    fn new(
        work: Work,
        clock: helix_runtime::Clock,
        ingress: crate::runtime::RuntimeIngress,
    ) -> AutoSaveHandler {
        AutoSaveHandler {
            save_pending: Default::default(),
            armed: Default::default(),
            debouncer: crate::runtime::RuntimeTaskDebouncer::new(
                Duration::from_millis(1),
                work,
                clock,
                ingress,
            ),
        }
    }

    fn event(&mut self, event: AutoSaveEvent) {
        match event {
            AutoSaveEvent::DocumentChanged { save_after } => {
                self.armed.store(true, atomic::Ordering::Relaxed);
                let save_pending = self.save_pending.clone();
                let armed = self.armed.clone();
                self.debouncer
                    .send_after_with(Duration::from_millis(save_after), move || {
                        armed.store(false, atomic::Ordering::Relaxed);
                        Some(RuntimeTaskEvent::AutoSaveRun { save_pending })
                    });
            }
            AutoSaveEvent::LeftInsertMode => {
                if !self.armed.load(atomic::Ordering::Relaxed)
                    && self.save_pending.load(atomic::Ordering::Relaxed)
                {
                    self.debouncer.send_now(RuntimeTaskEvent::AutoSaveRun {
                        save_pending: self.save_pending.clone(),
                    });
                }
            }
        }
    }

    pub fn spawn(
        runtime: Runtime,
        ingress: crate::runtime::RuntimeIngress,
    ) -> helix_runtime::Sender<AutoSaveEvent> {
        let (tx, mut rx) = helix_runtime::channel(128);
        let work = runtime.work().clone();
        let clock = runtime.clock().clone();
        work.clone()
            .spawn(async move {
                let mut handler = AutoSaveHandler::new(work, clock, ingress);
                while let Some(event) = rx.recv().await {
                    handler.event(event);
                }
                handler.debouncer.cancel();
            })
            .detach();
        tx
    }
}

pub(super) fn attach(editor: &helix_view::Editor, handlers: &Handlers) {
    let tx = handlers.auto_save.clone();
    editor.lifecycle().on_document_change(move |event| {
        let hook_start = std::time::Instant::now();
        let config = event.doc.config.load();
        if config.auto_save.after_delay.enable {
            send_blocking(
                &tx,
                AutoSaveEvent::DocumentChanged {
                    save_after: config.auto_save.after_delay.timeout,
                },
            );
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
