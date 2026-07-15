use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use helix_lsp::lsp;
use helix_runtime::{PulseGate, PulseHandle, Token, Work};

use helix_view::Editor;

use super::LspCompletionItem;
use crate::handlers::completion::CompletionItem;
use crate::runtime::send_ui_command_with;
use crate::runtime::ui::{CompletionCommand, UiCommand};

#[derive(Clone)]
pub struct ResolveRuntime {
    work: Work,
    clock: helix_runtime::Clock,
}

impl ResolveRuntime {
    pub fn new(runtime: &helix_runtime::Runtime) -> Self {
        Self {
            work: runtime.work().clone(),
            clock: runtime.clock().clone(),
        }
    }
}

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
    resolver: ResolveSender,
}

impl ResolveHandler {
    pub fn new(runtime: ResolveRuntime, ingress: crate::runtime::RuntimeIngress) -> ResolveHandler {
        let ResolveRuntime { work, clock } = runtime;
        ResolveHandler {
            last_request: None,
            resolver: ResolveTimeout::spawn(work, clock, ingress),
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
        let Some(ls) = editor.language_server_client(item.provider).cloned() else {
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
            self.resolver.resolve(PendingResolve { item, ls });
        } else {
            item.resolved = true;
        }
    }
}

impl Drop for ResolveHandler {
    fn drop(&mut self) {
        self.resolver.cancel();
    }
}

struct PendingResolve {
    item: Arc<LspCompletionItem>,
    ls: Arc<helix_lsp::Client>,
}

struct LatestRequest<T> {
    value: Option<T>,
    cancel_requested: bool,
}

impl<T> Default for LatestRequest<T> {
    fn default() -> Self {
        Self {
            value: None,
            cancel_requested: false,
        }
    }
}

impl<T> LatestRequest<T> {
    fn replace(&mut self, value: T) {
        self.value = Some(value);
    }

    fn cancel(&mut self) {
        self.value = None;
        self.cancel_requested = true;
    }

    fn take(&mut self) -> (bool, Option<T>) {
        let cancel_requested = std::mem::take(&mut self.cancel_requested);
        (cancel_requested, self.value.take())
    }
}

enum ResolveWake {}

#[derive(Clone)]
struct ResolveSender {
    latest: Arc<Mutex<LatestRequest<PendingResolve>>>,
    wake: PulseHandle<ResolveWake>,
}

impl ResolveSender {
    fn resolve(&self, request: PendingResolve) {
        self.latest
            .lock()
            .expect("completion resolve inbox lock poisoned")
            .replace(request);
        self.wake.request();
    }

    fn cancel(&self) {
        self.latest
            .lock()
            .expect("completion resolve inbox lock poisoned")
            .cancel();
        self.wake.request();
    }
}

struct ResolveTimeout {
    next_request: Option<PendingResolve>,
    in_flight: Option<Arc<LspCompletionItem>>,
    cancel: Option<Token>,
    deadline: Option<Instant>,
    work: Work,
    clock: helix_runtime::Clock,
    ingress: crate::runtime::RuntimeIngress,
}

impl ResolveTimeout {
    fn spawn(
        work: Work,
        clock: helix_runtime::Clock,
        ingress: crate::runtime::RuntimeIngress,
    ) -> ResolveSender {
        let mut gate = PulseGate::<ResolveWake>::new();
        let wake = gate.handle();
        let mut wake_rx = gate.take_receiver();
        let latest = Arc::new(Mutex::new(LatestRequest::default()));
        let inbox = latest.clone();
        let mut timeout = Self {
            next_request: None,
            in_flight: None,
            cancel: None,
            deadline: None,
            work,
            clock,
            ingress,
        };
        timeout
            .work
            .clone()
            .spawn(async move {
                timeout.run(&mut wake_rx, inbox).await;
                timeout.cancel();
            })
            .detach();
        ResolveSender { latest, wake }
    }

    async fn run(
        &mut self,
        wake_rx: &mut helix_runtime::PulseReceiver<ResolveWake>,
        inbox: Arc<Mutex<LatestRequest<PendingResolve>>>,
    ) {
        loop {
            if let Some(deadline) = self.deadline {
                let mut timer = self.clock.timer_at(deadline);
                tokio::select! {
                    biased;
                    wake = wake_rx.recv() => {
                        if wake.is_none() { break; }
                        self.drain_inbox(&inbox);
                    }
                    _ = &mut timer => {
                        self.deadline = None;
                        self.start();
                    }
                }
            } else {
                if wake_rx.recv().await.is_none() {
                    break;
                }
                self.drain_inbox(&inbox);
            }
        }
    }

    fn drain_inbox(&mut self, inbox: &Mutex<LatestRequest<PendingResolve>>) {
        let (cancel_requested, request) = inbox
            .lock()
            .expect("completion resolve inbox lock poisoned")
            .take();
        if cancel_requested {
            self.cancel();
        }
        if let Some(request) = request {
            self.handle_resolve(request);
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
            self.deadline = None;
            return;
        }

        self.next_request = Some(request);
        self.deadline = Some(self.clock.deadline_after(Duration::from_millis(150)));
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
        self.work
            .spawn(request.execute(cancel, self.ingress.clone()))
            .detach();
    }

    fn cancel(&mut self) {
        self.next_request = None;
        self.deadline = None;
        if let Some(cancel) = self.cancel.take() {
            cancel.cancel();
        }
        self.in_flight = None;
    }
}

impl PendingResolve {
    async fn execute(self, cancel: Token, ingress: crate::runtime::RuntimeIngress) {
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
        send_ui_command_with(
            UiCommand::Completion(Box::new(CompletionCommand::ReplaceResolvedItem {
                previous,
                resolved: Box::new(resolved_item),
            })),
            ingress,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::LatestRequest;

    #[test]
    fn resolve_inbox_keeps_only_the_latest_request() {
        let mut inbox = LatestRequest::default();
        inbox.replace(1);
        inbox.replace(2);

        assert_eq!(inbox.take(), (false, Some(2)));
    }

    #[test]
    fn cancellation_clears_pending_resolve() {
        let mut inbox = LatestRequest::default();
        inbox.replace(1);
        inbox.cancel();

        assert_eq!(inbox.take(), (true, None));
    }
}
