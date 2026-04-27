use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use futures_util::Future;
use helix_core::completion::CompletionProvider;
use helix_lsp::lsp;
use helix_lsp::util::pos_to_lsp_pos;
use helix_runtime::{Clock, Latest, Runtime, Work};
use helix_view::document::SavePoint;
use helix_view::handlers::completion::{CompletionEvent, ResponseContext};
use helix_view::{Document, DocumentId, ViewId};

use crate::config::Config;
use crate::handlers::completion::item::CompletionResponse;
use crate::handlers::completion::CompletionItems;
use crate::runtime::ui::{CompletionCommand, UiCommand};
use crate::runtime::{send_ui_command_with, RuntimeEvent};

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum TriggerKind {
    Auto,
    TriggerChar,
    Manual,
}

#[derive(Debug, Clone, Copy)]
pub struct Trigger {
    pub pos: usize,
    pub view: ViewId,
    pub doc: DocumentId,
    pub kind: TriggerKind,
}

#[derive(Debug)]
pub struct CompletionHandler {
    trigger: Option<Trigger>,
    config: Arc<ArcSwap<Config>>,
    latest: Latest,
    work: Work,
    clock: Clock,
    ingress: helix_runtime::Sender<RuntimeEvent>,
}

impl CompletionHandler {
    fn new(
        config: Arc<ArcSwap<Config>>,
        work: Work,
        clock: Clock,
        ingress: helix_runtime::Sender<RuntimeEvent>,
    ) -> CompletionHandler {
        Self {
            config,
            trigger: None,
            latest: Latest::default(),
            work,
            clock,
            ingress,
        }
    }

    fn schedule(&mut self, trigger: Trigger, delay: Duration) {
        let clock = self.clock.clone();
        let ingress = self.ingress.clone();
        self.latest
            .restart_with(&self.work, move |token| async move {
                let _ = clock.timer(delay).await;
                if token.is_canceled() {
                    return;
                }
                send_ui_command_with(
                    UiCommand::Completion(Box::new(CompletionCommand::RequestDebounced {
                        trigger,
                    })),
                    ingress,
                )
                .await;
            });
    }

    fn event(&mut self, event: CompletionEvent) {
        match event {
            CompletionEvent::AutoTrigger {
                cursor: trigger_pos,
                doc,
                view,
            } => {
                self.trigger = Some(Trigger {
                    pos: trigger_pos,
                    view,
                    doc,
                    kind: TriggerKind::Auto,
                });
                let delay = self.config.load().editor.completion_timeout;
                self.schedule(self.trigger.expect("trigger set"), delay);
            }
            CompletionEvent::TriggerChar { cursor, doc, view } => {
                self.trigger = Some(Trigger {
                    pos: cursor,
                    view,
                    doc,
                    kind: TriggerKind::TriggerChar,
                });
                self.schedule(self.trigger.expect("trigger set"), Duration::from_millis(5));
            }
            CompletionEvent::ManualTrigger { cursor, doc, view } => {
                self.trigger = Some(Trigger {
                    pos: cursor,
                    view,
                    doc,
                    kind: TriggerKind::Manual,
                });
                self.schedule(self.trigger.expect("trigger set"), Duration::ZERO);
            }
            CompletionEvent::Cancel => {
                self.trigger = None;
                self.latest.cancel();
            }
            CompletionEvent::DeleteText { cursor } => {
                if matches!(self.trigger, Some(Trigger{ pos, .. }) if cursor < pos) {
                    self.trigger = None;
                    self.latest.cancel();
                }
            }
        }
    }

    pub fn spawn(
        config: Arc<ArcSwap<Config>>,
        runtime: Runtime,
        ingress: helix_runtime::Sender<RuntimeEvent>,
    ) -> helix_runtime::Sender<CompletionEvent> {
        let (tx, mut rx) = helix_runtime::channel(128);
        let work = runtime.work().clone();
        let clock = runtime.clock().clone();
        work.clone()
            .spawn(async move {
                let mut handler = CompletionHandler::new(config, work, clock, ingress);
                while let Some(event) = rx.recv().await {
                    handler.event(event);
                }
            })
            .detach();
        tx
    }
}

pub(crate) fn request_completions_from_language_server(
    ls: &helix_lsp::Client,
    doc: &Document,
    view: ViewId,
    context: lsp::CompletionContext,
    priority: i8,
    savepoint: Arc<SavePoint>,
) -> impl Future<Output = CompletionResponse> {
    let provider = ls.id();
    let offset_encoding = ls.offset_encoding();
    let text = doc.text();
    let cursor = doc.selection(view).primary().cursor(text.slice(..));
    let pos = pos_to_lsp_pos(text, cursor, offset_encoding);
    let doc_id = doc.identifier();

    // it's important that this is before the async block (and that this is not an async function)
    // to ensure the request is dispatched right away before any new edit notifications
    let completion_response = ls.completion(doc_id, pos, None, context).unwrap();
    async move {
        let response: Option<lsp::CompletionResponse> = completion_response
            .await
            .inspect_err(|err| log::error!("completion request failed: {err}"))
            .ok()
            .flatten();
        let (mut items, is_incomplete) = match response {
            Some(lsp::CompletionResponse::Array(items)) => (items, false),
            Some(lsp::CompletionResponse::List(lsp::CompletionList {
                is_incomplete,
                items,
            })) => (items, is_incomplete),
            None => (Vec::new(), false),
        };
        items.sort_by(|item1, item2| {
            let sort_text1 = item1.sort_text.as_deref().unwrap_or(&item1.label);
            let sort_text2 = item2.sort_text.as_deref().unwrap_or(&item2.label);
            sort_text1.cmp(sort_text2)
        });
        CompletionResponse {
            items: CompletionItems::Lsp(items),
            context: ResponseContext {
                is_incomplete,
                priority,
                savepoint,
            },
            provider: CompletionProvider::Lsp(provider),
        }
    }
}
