use std::time::Duration;

use helix_event::register_hook;
use helix_runtime::{Clock, Debounce, Runtime, Work};
use helix_view::{
    events::{DocumentDidOpen, EditorConfigDidChange},
    handlers::{BlameEvent, Handlers},
};

use crate::runtime::{send_task_event_with, RuntimeEvent, RuntimeTaskEvent};

#[derive(Debug)]
pub struct BlameHandler {
    debounce: Debounce,
    work: Work,
    clock: Clock,
    ingress: helix_runtime::Sender<RuntimeEvent>,
}

impl BlameHandler {
    fn new(work: Work, clock: Clock, ingress: helix_runtime::Sender<RuntimeEvent>) -> Self {
        Self {
            debounce: Debounce::new(Duration::from_millis(50)),
            work,
            clock,
            ingress,
        }
    }

    fn event(&mut self, event: BlameEvent) {
        let BlameEvent { path, doc_id, line } = event;
        let ingress = self.ingress.clone();
        self.debounce.restart(&self.work, &self.clock, async move {
            send_task_event_with(
                RuntimeTaskEvent::BlameFetchDebounced { doc_id, path, line },
                ingress,
            )
            .await;
        });
    }

    pub fn spawn(
        runtime: Runtime,
        ingress: helix_runtime::Sender<RuntimeEvent>,
    ) -> helix_runtime::Sender<BlameEvent> {
        let (tx, mut rx) = helix_runtime::channel(128);
        let work = runtime.work().clone();
        let clock = runtime.clock().clone();
        work.clone().spawn(async move {
            let mut handler = BlameHandler::new(work, clock, ingress);
            while let Some(event) = rx.recv().await {
                handler.event(event);
            }
        }).detach();
        tx
    }
}

pub(super) fn register_hooks(handlers: &Handlers) {
    let tx = handlers.blame.clone();
    register_hook!(move |event: &mut DocumentDidOpen<'_>| {
        if event.editor.config().inline_blame.auto_fetch {
            helix_runtime::send_blocking(
                &tx,
                BlameEvent {
                    path: event.path.to_path_buf(),
                    doc_id: event.doc,
                    line: None,
                },
            );
        }
        Ok(())
    });
    let tx = handlers.blame.clone();
    register_hook!(move |event: &mut EditorConfigDidChange<'_>| {
        let has_enabled_inline_blame = !event.old_config.inline_blame.auto_fetch
            && event.editor.config().inline_blame.auto_fetch;

        if has_enabled_inline_blame {
            // request blame for all documents, since any of them could have
            // outdated blame
            for doc in event.editor.documents() {
                if let Some(path) = doc.path() {
                    helix_runtime::send_blocking(
                        &tx,
                        BlameEvent {
                            path: path.to_path_buf(),
                            doc_id: doc.id(),
                            line: None,
                        },
                    );
                }
            }
        }
        Ok(())
    });
}
