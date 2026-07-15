use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use helix_runtime::Runtime;
use helix_view::handlers::{BlameEvent, Handlers};
use helix_view::DocumentId;

use crate::runtime::RuntimeTaskEvent;

#[derive(Debug)]
pub struct BlameHandler {
    pending: HashMap<DocumentId, BlameEvent>,
    deadline: Option<Instant>,
    clock: helix_runtime::Clock,
    ingress: crate::runtime::RuntimeIngress,
}

fn merge_pending_blame(pending: &mut HashMap<DocumentId, BlameEvent>, event: BlameEvent) {
    match pending.entry(event.doc_id) {
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(event);
        }
        std::collections::hash_map::Entry::Occupied(mut entry) => {
            // A status-line request carries a line and must not be erased by a
            // later opportunistic inline-blame refresh for the same document.
            if entry.get().line.is_none() || event.line.is_some() {
                entry.insert(event);
            }
        }
    }
}

impl BlameHandler {
    fn new(clock: helix_runtime::Clock, ingress: crate::runtime::RuntimeIngress) -> Self {
        Self {
            pending: HashMap::new(),
            deadline: None,
            clock,
            ingress,
        }
    }

    fn event(&mut self, event: BlameEvent) {
        merge_pending_blame(&mut self.pending, event);
        self.deadline = Some(self.clock.deadline_after(Duration::from_millis(50)));
    }

    async fn flush(&mut self) {
        for (_, BlameEvent { path, doc_id, line }) in std::mem::take(&mut self.pending) {
            let _ = self
                .ingress
                .send_task(RuntimeTaskEvent::BlameFetchDebounced { doc_id, path, line })
                .await;
        }
    }

    async fn run(mut self, mut rx: helix_runtime::Receiver<BlameEvent>) {
        loop {
            if let Some(deadline) = self.deadline {
                let mut timer = self.clock.timer_at(deadline);
                tokio::select! {
                    biased;
                    event = rx.recv() => {
                        let Some(event) = event else { break };
                        self.event(event);
                    }
                    _ = &mut timer => {
                        self.deadline = None;
                        self.flush().await;
                    }
                }
            } else {
                let Some(event) = rx.recv().await else { break };
                self.event(event);
            }
        }
    }

    pub fn spawn(
        runtime: Runtime,
        ingress: crate::runtime::RuntimeIngress,
    ) -> helix_runtime::Sender<BlameEvent> {
        let (tx, rx) = helix_runtime::channel(128);
        let work = runtime.work().clone();
        let clock = runtime.clock().clone();
        work.clone()
            .spawn(async move {
                BlameHandler::new(clock, ingress).run(rx).await;
            })
            .detach();
        tx
    }
}

pub(super) fn attach(editor: &helix_view::Editor, handlers: &Handlers) {
    let requests = handlers.blame.clone();
    let open_requests = requests.clone();
    editor.lifecycle().on_document_open(move |event| {
        if event.editor.config().inline_blame.auto_fetch {
            open_requests.send(BlameEvent {
                path: event.path.to_path_buf(),
                doc_id: event.doc,
                line: None,
            });
        }
        Ok(())
    });
    editor.lifecycle().on_editor_config_change(move |event| {
        let has_enabled_inline_blame = !event.old_config.inline_blame.auto_fetch
            && event.editor.config().inline_blame.auto_fetch;

        if has_enabled_inline_blame {
            // request blame for all documents, since any of them could have
            // outdated blame
            for doc in event.editor.documents() {
                if let Some(path) = doc.path() {
                    requests.send(BlameEvent {
                        path: path.to_path_buf(),
                        doc_id: doc.id(),
                        line: None,
                    });
                }
            }
        }
        Ok(())
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn automatic_refresh_does_not_erase_pending_line_request() {
        let doc_id = DocumentId::default();
        let mut pending = HashMap::new();
        merge_pending_blame(
            &mut pending,
            BlameEvent {
                path: PathBuf::from("old"),
                doc_id,
                line: None,
            },
        );
        merge_pending_blame(
            &mut pending,
            BlameEvent {
                path: PathBuf::from("manual"),
                doc_id,
                line: Some(12),
            },
        );
        merge_pending_blame(
            &mut pending,
            BlameEvent {
                path: PathBuf::from("automatic"),
                doc_id,
                line: None,
            },
        );

        let request = pending.get(&doc_id).expect("request should be retained");
        assert_eq!(request.path, PathBuf::from("manual"));
        assert_eq!(request.line, Some(12));
    }
}
