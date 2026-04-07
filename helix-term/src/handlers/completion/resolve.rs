use std::sync::Arc;
use std::time::Duration;

use helix_lsp::lsp;
use helix_runtime::{send_blocking, Debounce, Runtime, Token, Work};
use helix_runtime::Sender as IngressSender;

use helix_view::Editor;

use super::LspCompletionItem;
use crate::handlers::completion::CompletionItem;
use crate::runtime::{send_ui_command_with, RuntimeEvent};
use crate::runtime::ui::{CompletionCommand, UiCommand};

/// A hook for resolving incomplete completion items.
///
/// From the [LSP spec](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_completion):
///
/// > If computing full completion items is expensive, servers can additionally provide a
/// > handler for the completion item resolve request. ...
/// > A typical use case is for example: the `textDocument/completion` request doesn't fill
/// > in the `documentation` property for returned completion items since it is expensive
/// > to compute. When the item is selected in the user interface then a
/// > 'completionItem/resolve' request is sent with the selected completion item as a parameter.
/// > The returned completion item should have the documentation property filled in.
pub struct ResolveHandler {
    last_request: Option<Arc<LspCompletionItem>>,
    resolver: helix_runtime::Sender<ResolveRequest>,
}

impl ResolveHandler {
    pub fn new(runtime: Runtime, ingress: IngressSender<RuntimeEvent>) -> ResolveHandler {
        ResolveHandler {
            last_request: None,
            resolver: ResolveTimeout::spawn(runtime.work().clone(), runtime.clock().clone(), ingress),
        }
    }

    pub fn ensure_item_resolved(&mut self, editor: &mut Editor, item: &mut LspCompletionItem) {
        if item.resolved {
            return;
        }
        // We consider an item to be fully resolved if it has non-empty, none-`None` details,
        // docs and additional text-edits. Ideally we could use `is_some` instead of this
        // check but some language servers send values like `Some([])` for additional text
        // edits although the items need to be resolved. This is probably a consequence of
        // how `null` works in the JavaScript world.
        let is_resolved = item
            .item
            .documentation
            .as_ref()
            .is_some_and(|docs| match docs {
                lsp::Documentation::String(text) => !text.is_empty(),
                lsp::Documentation::MarkupContent(markup) => !markup.value.is_empty(),
            })
            && item
                .item
                .detail
                .as_ref()
                .is_some_and(|detail| !detail.is_empty())
            && item
                .item
                .additional_text_edits
                .as_ref()
                .is_some_and(|edits| !edits.is_empty());
        if is_resolved {
            item.resolved = true;
            return;
        }
        if self.last_request.as_deref().is_some_and(|it| it == item) {
            return;
        }
        let Some(ls) = editor.language_servers.get_by_id(item.provider).cloned() else {
            item.resolved = true;
            return;
        };
        if matches!(
            ls.capabilities().completion_provider,
            Some(lsp::CompletionOptions {
                resolve_provider: Some(true),
                ..
            })
        ) {
            let item = Arc::new(item.clone());
            self.last_request = Some(item.clone());
            send_blocking(&self.resolver, ResolveRequest::Resolve { item, ls })
        } else {
            item.resolved = true;
        }
    }
}

impl Drop for ResolveHandler {
    fn drop(&mut self) {
        let _ = self.resolver.try_send(ResolveRequest::Cancel);
    }
}

enum ResolveRequest {
    Resolve {
        item: Arc<LspCompletionItem>,
        ls: Arc<helix_lsp::Client>,
    },
    Start,
    Cancel,
}

struct PendingResolve {
    item: Arc<LspCompletionItem>,
    ls: Arc<helix_lsp::Client>,
    ingress: IngressSender<RuntimeEvent>,
}

struct ResolveTimeout {
    next_request: Option<PendingResolve>,
    in_flight: Option<Arc<LspCompletionItem>>,
    cancel: Option<Token>,
    debounce: Debounce,
    work: Work,
    clock: helix_runtime::Clock,
    tx: helix_runtime::Sender<ResolveRequest>,
    ingress: IngressSender<RuntimeEvent>,
}

impl ResolveTimeout {
    fn spawn(
        work: Work,
        clock: helix_runtime::Clock,
        ingress: IngressSender<RuntimeEvent>,
    ) -> helix_runtime::Sender<ResolveRequest> {
        let (tx, mut rx) = helix_runtime::channel(128);
        let mut timeout = Self {
            next_request: None,
            in_flight: None,
            cancel: None,
            debounce: Debounce::new(Duration::from_millis(150)),
            work,
            clock,
            tx: tx.clone(),
            ingress,
        };
        timeout.work.clone().spawn(async move {
            while let Some(request) = rx.recv().await {
                timeout.event(request);
            }
            timeout.cancel();
        }).detach();
        tx
    }

    fn event(&mut self, request: ResolveRequest) {
        match request {
            ResolveRequest::Resolve { item, ls } => {
                self.handle_resolve(PendingResolve {
                    item,
                    ls,
                    ingress: self.ingress.clone(),
                })
            }
            ResolveRequest::Start => self.start(),
            ResolveRequest::Cancel => self.cancel(),
        }
    }

    fn handle_resolve(&mut self, request: PendingResolve) {
        if self
            .next_request
            .as_ref()
            .is_some_and(|old_request| old_request.item == request.item)
        {
            return;
        }
        if self
            .in_flight
            .as_ref()
            .is_some_and(|old_request| old_request == &request.item)
        {
            self.next_request = None;
            self.debounce.cancel();
            return;
        }

        self.next_request = Some(request);
        let tx = self.tx.clone();
        self.debounce.restart(&self.work, &self.clock, async move {
            let _ = tx.send(ResolveRequest::Start).await;
        });
    }

    fn start(&mut self) {
        let Some(request) = self.next_request.take() else {
            return;
        };

        let cancel = Token::new();
        if let Some(current) = self.cancel.replace(cancel.clone()) {
            current.cancel();
        }
        self.in_flight = Some(request.item.clone());
        self.work.spawn(request.execute(cancel)).detach();
    }

    fn cancel(&mut self) {
        self.next_request = None;
        self.debounce.cancel();
        if let Some(cancel) = self.cancel.take() {
            cancel.cancel();
        }
        self.in_flight = None;
    }
}

impl PendingResolve {
    async fn execute(self, cancel: Token) {
        let future = self.ls.resolve_completion_item(&self.item.item);
        let resolved_item = tokio::select! {
            _ = cancel.canceled() => return,
            resolved_item = future => resolved_item,
        };
        let previous = self.item.clone();
        let resolved_item = CompletionItem::Lsp(match resolved_item {
            Ok(item) => LspCompletionItem {
                item,
                resolved: true,
                ..*previous
            },
            Err(err) => {
                log::error!("completion resolve request failed: {err}");
                // set item to resolved so we don't request it again
                // we could also remove it but that oculd be odd ui
                let mut item = (*previous).clone();
                item.resolved = true;
                item
            }
        });
        send_ui_command_with(UiCommand::Completion(
            CompletionCommand::ReplaceResolvedItem {
                previous,
                resolved: resolved_item,
            },
        ), self.ingress)
        .await
    }
}
