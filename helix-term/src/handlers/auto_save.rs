use std::{
    sync::{
        atomic::{self, AtomicBool},
        Arc,
    },
    time::Duration,
};

use anyhow::Ok;
use arc_swap::access::Access;

use crate::{events::OnModeSwitch, runtime::{send_task_event_with, RuntimeEvent, RuntimeTaskEvent}};
use helix_event::register_hook;
use helix_runtime::{send_blocking, Clock, Debounce, Runtime, Work};
use helix_view::bench::log_command_phase;
use helix_view::{
    document::Mode,
    events::DocumentDidChange,
    handlers::{AutoSaveEvent, Handlers},
};

#[derive(Debug)]
pub(super) struct AutoSaveHandler {
    save_pending: Arc<AtomicBool>,
    armed: Arc<AtomicBool>,
    debounce: Debounce,
    work: Work,
    clock: Clock,
    ingress: helix_runtime::Sender<RuntimeEvent>,
}

impl AutoSaveHandler {
    fn new(work: Work, clock: Clock, ingress: helix_runtime::Sender<RuntimeEvent>) -> AutoSaveHandler {
        AutoSaveHandler {
            save_pending: Default::default(),
            armed: Default::default(),
            debounce: Debounce::new(Duration::from_millis(1)),
            work,
            clock,
            ingress,
        }
    }

    fn event(&mut self, event: AutoSaveEvent) {
        match event {
            AutoSaveEvent::DocumentChanged { save_after } => {
                self.armed.store(true, atomic::Ordering::Relaxed);
                let save_pending = self.save_pending.clone();
                let armed = self.armed.clone();
                let ingress = self.ingress.clone();
                self.debounce = Debounce::new(Duration::from_millis(save_after));
                self.debounce.restart(&self.work, &self.clock, async move {
                    armed.store(false, atomic::Ordering::Relaxed);
                    send_task_event_with(RuntimeTaskEvent::AutoSaveRun { save_pending }, ingress)
                        .await;
                });
            }
            AutoSaveEvent::LeftInsertMode => {
                if !self.armed.load(atomic::Ordering::Relaxed)
                    && self.save_pending.load(atomic::Ordering::Relaxed)
                {
                    let save_pending = self.save_pending.clone();
                    let ingress = self.ingress.clone();
                    self.work
                        .spawn(async move {
                            send_task_event_with(RuntimeTaskEvent::AutoSaveRun { save_pending }, ingress)
                            .await;
                        })
                        .detach();
                }
            }
        }
    }

    pub fn spawn(
        runtime: Runtime,
        ingress: helix_runtime::Sender<RuntimeEvent>,
    ) -> helix_runtime::Sender<AutoSaveEvent> {
        let (tx, mut rx) = helix_runtime::channel(128);
        let work = runtime.work().clone();
        let clock = runtime.clock().clone();
        work.clone().spawn(async move {
            let mut handler = AutoSaveHandler::new(work, clock, ingress);
            while let Some(event) = rx.recv().await {
                handler.event(event);
            }
        }).detach();
        tx
    }
}

pub(super) fn register_hooks(handlers: &Handlers) {
    let tx = handlers.auto_save.clone();
    register_hook!(move |event: &mut DocumentDidChange<'_>| {
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

    let tx = handlers.auto_save.clone();
    register_hook!(move |event: &mut OnModeSwitch<'_, '_>| {
        if event.old_mode == Mode::Insert {
            send_blocking(&tx, AutoSaveEvent::LeftInsertMode)
        }
        Ok(())
    });
}
