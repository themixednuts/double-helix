use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use helix_core::syntax::config::LanguageServerFeature;
use helix_lsp::lsp;
use helix_runtime::{Runtime, Token};
use helix_stdx::rope::RopeSliceExt;
use helix_view::bench::log_command_phase;
use helix_view::handlers::lsp::{SignatureHelpEvent, SignatureHelpInvoked, SignatureHelpRequestId};

use crate::handlers::Handlers;
use crate::runtime::RuntimeTaskEvent;

#[derive(Debug, PartialEq, Eq)]
enum State {
    Open,
    Closed,
    Pending,
}

/// debounce timeout in ms, value taken from VSCode
/// TODO: make this configurable?
const TIMEOUT: u64 = 120;

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub(super) struct SignatureHelpHandler {
    trigger: Option<SignatureHelpInvoked>,
    state: State,
    request: Option<SignatureHelpRequestId>,
    cancel: Option<Token>,
    pending_task: Option<RuntimeTaskEvent>,
    deadline: Option<Instant>,
    clock: helix_runtime::Clock,
    ingress: crate::runtime::RuntimeIngress,
}

impl SignatureHelpHandler {
    fn new(
        clock: helix_runtime::Clock,
        ingress: crate::runtime::RuntimeIngress,
    ) -> SignatureHelpHandler {
        SignatureHelpHandler {
            trigger: None,
            state: State::Closed,
            request: None,
            cancel: None,
            pending_task: None,
            deadline: None,
            clock,
            ingress,
        }
    }

    fn next_request() -> SignatureHelpRequestId {
        loop {
            let raw = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
            if let Some(id) = std::num::NonZeroU64::new(raw) {
                return SignatureHelpRequestId::new(id);
            }
        }
    }

    fn dispatch_request(
        &mut self,
        invocation: SignatureHelpInvoked,
        trigger_kind: lsp::SignatureHelpTriggerKind,
        is_retrigger: bool,
        delay: bool,
    ) -> Option<RuntimeTaskEvent> {
        let request = Self::next_request();
        let cancel = Token::new();
        if let Some(current) = self.cancel.replace(cancel.clone()) {
            current.cancel();
        }
        self.request = Some(request);
        self.state = State::Pending;
        let event = RuntimeTaskEvent::RequestSignatureDebounced {
            invoked: invocation,
            request,
            trigger_kind,
            is_retrigger,
            cancel,
        };

        if delay {
            self.pending_task = Some(event);
            self.deadline = Some(self.clock.deadline_after(Duration::from_millis(TIMEOUT)));
            None
        } else {
            self.pending_task = None;
            self.deadline = None;
            Some(event)
        }
    }

    fn event(&mut self, event: SignatureHelpEvent) -> Option<RuntimeTaskEvent> {
        match event {
            SignatureHelpEvent::Invoked => {
                self.trigger = Some(SignatureHelpInvoked::Manual);
                self.state = State::Closed;
                return self.dispatch_request(
                    SignatureHelpInvoked::Manual,
                    lsp::SignatureHelpTriggerKind::INVOKED,
                    false,
                    false,
                );
            }
            SignatureHelpEvent::Trigger => {
                let is_retrigger = !matches!(self.state, State::Closed);
                if self.trigger.is_none() {
                    self.trigger = Some(SignatureHelpInvoked::Automatic)
                }
                let invocation = self.trigger.take().unwrap();
                return self.dispatch_request(
                    invocation,
                    lsp::SignatureHelpTriggerKind::TRIGGER_CHARACTER,
                    is_retrigger,
                    true,
                );
            }
            SignatureHelpEvent::ReTrigger => {
                // don't retrigger if we aren't open/pending yet
                if matches!(self.state, State::Closed) {
                    return None;
                }
            }
            SignatureHelpEvent::Cancel => {
                self.state = State::Closed;
                self.pending_task = None;
                self.deadline = None;
                if let Some(cancel) = self.cancel.take() {
                    cancel.cancel();
                }
                self.request = None;
                return None;
            }
            SignatureHelpEvent::RequestComplete { request, open } => {
                if self.request != Some(request) {
                    return None;
                }
                self.state = if open { State::Open } else { State::Closed };
                self.cancel = None;
                self.request = None;
                return None;
            }
        }
        if self.trigger.is_none() {
            self.trigger = Some(SignatureHelpInvoked::Automatic)
        }
        let invocation = self.trigger.take().unwrap();
        self.dispatch_request(
            invocation,
            lsp::SignatureHelpTriggerKind::CONTENT_CHANGE,
            true,
            true,
        )
    }

    async fn send_task(&self, task: RuntimeTaskEvent) {
        let _ = self.ingress.send_task(task).await;
    }

    async fn run(mut self, mut rx: helix_runtime::Receiver<SignatureHelpEvent>) {
        loop {
            if let Some(deadline) = self.deadline {
                let mut timer = self.clock.timer_at(deadline);
                tokio::select! {
                    biased;
                    event = rx.recv() => {
                        let Some(event) = event else { break };
                        if let Some(task) = self.event(event) {
                            self.send_task(task).await;
                        }
                    }
                    _ = &mut timer => {
                        self.deadline = None;
                        if let Some(task) = self.pending_task.take() {
                            self.send_task(task).await;
                        }
                    }
                }
            } else {
                let Some(event) = rx.recv().await else { break };
                if let Some(task) = self.event(event) {
                    self.send_task(task).await;
                }
            }
        }

        if let Some(cancel) = self.cancel.take() {
            cancel.cancel();
        }
        self.request = None;
    }

    pub fn spawn(
        runtime: Runtime,
        ingress: crate::runtime::RuntimeIngress,
    ) -> helix_runtime::Sender<SignatureHelpEvent> {
        let (tx, rx) = helix_runtime::channel(128);
        let work = runtime.work().clone();
        let clock = runtime.clock().clone();
        work.clone()
            .spawn(async move {
                SignatureHelpHandler::new(clock, ingress).run(rx).await;
            })
            .detach();
        tx
    }
}

pub(crate) fn signature_help_post_insert_char_hook(
    tx: &helix_view::handlers::SignatureHelpEvents,
    cx: &mut crate::commands::Context,
) -> anyhow::Result<()> {
    if !cx.editor.config().lsp.auto_signature_help {
        return Ok(());
    }
    let (view_id, doc) = focused!(cx.editor);
    // TODO support multiple language servers (not just the first that is found), likely by merging UI somehow
    let Some(language_server) = doc
        .language_servers_with_feature(LanguageServerFeature::SignatureHelp)
        .next()
    else {
        return Ok(());
    };

    let capabilities = language_server.capabilities();

    if let lsp::ServerCapabilities {
        signature_help_provider:
            Some(lsp::SignatureHelpOptions {
                trigger_characters: Some(triggers),
                // TODO: retrigger_characters
                ..
            }),
        ..
    } = capabilities
    {
        let mut text = doc.text().slice(..);
        let cursor = doc.selection(view_id).primary().cursor(text);
        text = text.slice(..cursor);
        if triggers.iter().any(|trigger| text.ends_with(trigger)) {
            tx.send(SignatureHelpEvent::Trigger)
        }
    }
    Ok(())
}

pub(super) fn attach(editor: &helix_view::Editor, handlers: &Handlers) {
    let retriggers = handlers.signature_hints.clone();
    let document_retriggers = retriggers.clone();
    editor.lifecycle().on_document_change(move |event| {
        let hook_start = std::time::Instant::now();
        if event.doc.config.load().lsp.auto_signature_help && !event.ghost_transaction {
            document_retriggers.send(SignatureHelpEvent::ReTrigger);
        }
        let hook_dur = hook_start.elapsed();
        log_command_phase(
            "document_did_change_hook",
            "signature_help",
            hook_dur,
            || {
                format!(
                    "doc_id={:?} ghost={} auto_signature_help={} lines={} bytes={}",
                    event.doc.id(),
                    event.ghost_transaction,
                    event.doc.config.load().lsp.auto_signature_help,
                    event.doc.text().len_lines(),
                    event.doc.text().len_bytes()
                )
            },
        );
        Ok(())
    });

    editor.lifecycle().on_selection_change(move |event| {
        if event.doc.config.load().lsp.auto_signature_help {
            retriggers.send(SignatureHelpEvent::ReTrigger);
        }
        Ok(())
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::FutureExt;

    #[test]
    fn idle_handler_yields_until_its_typed_event_mailbox_wakes() {
        let rt = helix_runtime::test::RuntimeTest::default();
        let runtime = rt.runtime();
        let (ingress, _ingress_rx) = crate::runtime::RuntimeIngress::channel(runtime.clone());
        let handler = SignatureHelpHandler::new(runtime.clock().clone(), ingress);
        let (tx, rx) = helix_runtime::channel(1);

        rt.block_on(async move {
            let run = handler.run(rx);
            futures_util::pin_mut!(run);
            assert!(run.as_mut().now_or_never().is_none());
            drop(tx);
            run.await;
        });
    }

    #[test]
    fn matching_completion_updates_pending_request_state() {
        let rt = helix_runtime::test::RuntimeTest::default();
        let runtime = rt.runtime();
        let (ingress, _ingress_rx) = crate::runtime::RuntimeIngress::channel(runtime.clone());
        let mut handler = SignatureHelpHandler::new(runtime.clock().clone(), ingress);
        let request = SignatureHelpHandler::next_request();
        handler.request = Some(request);
        handler.state = State::Pending;
        handler.cancel = Some(Token::new());

        assert!(handler
            .event(SignatureHelpEvent::RequestComplete {
                request,
                open: true,
            })
            .is_none());
        assert_eq!(handler.state, State::Open);
        assert_eq!(handler.request, None);
        assert!(handler.cancel.is_none());
    }
}
