use std::time::Duration;

use helix_runtime::{Runtime, Work};
use helix_view::handlers::{BlameEvent, Handlers};

use crate::runtime::RuntimeTaskEvent;

#[derive(Debug)]
pub struct BlameHandler {
    debouncer: crate::runtime::RuntimeTaskDebouncer,
}

impl BlameHandler {
    fn new(
        work: Work,
        clock: helix_runtime::Clock,
        ingress: crate::runtime::RuntimeIngress,
    ) -> Self {
        Self {
            debouncer: crate::runtime::RuntimeTaskDebouncer::new(
                Duration::from_millis(50),
                work,
                clock,
                ingress,
            ),
        }
    }

    fn event(&mut self, event: BlameEvent) {
        let BlameEvent { path, doc_id, line } = event;
        self.debouncer
            .send(RuntimeTaskEvent::BlameFetchDebounced { doc_id, path, line });
    }

    pub fn spawn(
        runtime: Runtime,
        ingress: crate::runtime::RuntimeIngress,
    ) -> helix_runtime::Sender<BlameEvent> {
        let (tx, mut rx) = helix_runtime::channel(128);
        let work = runtime.work().clone();
        let clock = runtime.clock().clone();
        work.clone()
            .spawn(async move {
                let mut handler = BlameHandler::new(work, clock, ingress);
                while let Some(event) = rx.recv().await {
                    handler.event(event);
                }
            })
            .detach();
        tx
    }
}

pub(super) fn attach(editor: &helix_view::Editor, handlers: &Handlers) {
    let tx = handlers.blame.clone();
    editor.lifecycle().on_document_open(move |event| {
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
    editor.lifecycle().on_editor_config_change(move |event| {
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
