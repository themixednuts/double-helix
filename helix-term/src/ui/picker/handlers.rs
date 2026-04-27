use std::{path::Path, sync::Arc, time::Duration};

use helix_runtime::{channel, Clock, Debounce, Sender, Work};

use crate::runtime::{ui::command::PickerCommand, RuntimeEvent, UiCommand};

use super::SharedIngress;

pub(super) struct PreviewHighlightHandler {
    trigger: Option<Arc<Path>>,
    debounce: Debounce,
    work: Work,
    clock: Clock,
    tx: Option<Sender<PreviewHighlightEvent>>,
    ingress: SharedIngress,
}

enum PreviewHighlightEvent {
    Set(Arc<Path>),
    Flush,
}

impl PreviewHighlightHandler {
    pub(super) fn spawn(work: Work, clock: Clock, ingress: SharedIngress) -> Sender<Arc<Path>> {
        let (tx, mut rx) = channel(128);
        let mut handler = Self {
            trigger: None,
            debounce: Debounce::new(Duration::from_millis(150)),
            work: work.clone(),
            clock,
            tx: None,
            ingress,
        };
        handler.tx = Some(tx.clone());
        handler
            .work
            .clone()
            .spawn(async move {
                while let Some(event) = rx.recv().await {
                    handler.handle_event(event);
                }
                handler.debounce.cancel();
            })
            .detach();
        let (frontend_tx, mut frontend_rx) = channel(128);
        work.spawn(async move {
            while let Some(path) = frontend_rx.recv().await {
                let _ = tx.send(PreviewHighlightEvent::Set(path)).await;
            }
        })
        .detach();
        frontend_tx
    }

    fn handle_event(&mut self, event: PreviewHighlightEvent) {
        match event {
            PreviewHighlightEvent::Set(path) => {
                if self
                    .trigger
                    .as_ref()
                    .is_some_and(|trigger| trigger == &path)
                {
                    return;
                }
                self.trigger = Some(path);
                self.restart();
            }
            PreviewHighlightEvent::Flush => self.finish_debounce(),
        }
    }

    fn restart(&mut self) {
        let tx = self
            .tx
            .as_ref()
            .expect("picker preview sender initialized")
            .clone();
        self.debounce.restart(&self.work, &self.clock, async move {
            let _ = tx.send(PreviewHighlightEvent::Flush).await;
        });
    }

    fn finish_debounce(&mut self) {
        let Some(path) = self.trigger.take() else {
            return;
        };
        let ingress = (*self.ingress).clone();

        helix_runtime::send_blocking(
            &ingress,
            RuntimeEvent::Ui(UiCommand::Picker(PickerCommand::RequestPreviewHighlight {
                path: path.to_path_buf(),
            })),
        );
    }
}

pub(super) struct DynamicQueryChange {
    pub query: Arc<str>,
    pub is_paste: bool,
}

enum DynamicQueryEvent {
    Change(DynamicQueryChange),
    Flush,
}

pub(super) struct DynamicQueryHandler {
    // Duration used as a debounce.
    // Defaults to 100ms if not provided via `Picker::with_dynamic_query`. Callers may want to set
    // this higher if the dynamic query is expensive - for example global search.
    debounce: Duration,
    timer: Debounce,
    work: Work,
    clock: Clock,
    tx: Option<Sender<DynamicQueryEvent>>,
    last_query: Arc<str>,
    query: Option<Arc<str>>,
    ingress: SharedIngress,
}

impl DynamicQueryHandler {
    pub(super) fn new(
        duration_ms: Option<u64>,
        work: Work,
        clock: Clock,
        ingress: SharedIngress,
    ) -> Self {
        Self {
            debounce: Duration::from_millis(duration_ms.unwrap_or(100)),
            timer: Debounce::new(Duration::from_millis(duration_ms.unwrap_or(100))),
            work,
            clock,
            tx: None,
            last_query: "".into(),
            query: None,
            ingress,
        }
    }

    pub(super) fn spawn(mut self) -> Sender<DynamicQueryChange> {
        let (tx, mut rx) = channel(128);
        self.tx = Some(tx.clone());
        let internal_tx = tx.clone();
        let work = self.work.clone();
        self.work
            .clone()
            .spawn(async move {
                while let Some(event) = rx.recv().await {
                    self.handle_event(event);
                }
                self.timer.cancel();
            })
            .detach();
        let (frontend_tx, mut frontend_rx) = channel(128);
        work.spawn(async move {
            while let Some(change) = frontend_rx.recv().await {
                let _ = internal_tx.send(DynamicQueryEvent::Change(change)).await;
            }
        })
        .detach();
        frontend_tx
    }

    fn handle_event(&mut self, event: DynamicQueryEvent) {
        match event {
            DynamicQueryEvent::Change(change) => {
                let DynamicQueryChange { query, is_paste } = change;
                if query == self.last_query {
                    self.query = None;
                    self.timer.cancel();
                    return;
                }
                self.query = Some(query);
                if is_paste {
                    self.timer.cancel();
                    self.finish_debounce();
                } else {
                    self.restart();
                }
            }
            DynamicQueryEvent::Flush => self.finish_debounce(),
        }
    }

    fn restart(&mut self) {
        let tx = self
            .tx
            .as_ref()
            .expect("picker dynamic query sender initialized")
            .clone();
        self.timer = Debounce::new(self.debounce);
        self.timer.restart(&self.work, &self.clock, async move {
            let _ = tx.send(DynamicQueryEvent::Flush).await;
        });
    }

    fn finish_debounce(&mut self) {
        let Some(query) = self.query.take() else {
            return;
        };
        self.last_query = query.clone();
        let ingress = (*self.ingress).clone();

        helix_runtime::send_blocking(
            &ingress,
            RuntimeEvent::Ui(UiCommand::Picker(PickerCommand::RunDynamicQuery { query })),
        );
    }
}
