use std::time::Duration;

use helix_core::syntax::config::LanguageServerFeature;
use helix_event::register_hook;
use helix_lsp::lsp;
use helix_runtime::{send_blocking, Clock, Debounce, Runtime, Token, Work};
use helix_stdx::rope::RopeSliceExt;
use helix_view::bench::log_command_phase;
use helix_view::document::Mode;
use helix_view::events::{DocumentDidChange, SelectionDidChange};
use helix_view::handlers::lsp::{SignatureHelpEvent, SignatureHelpInvoked, SignatureHelpRequestId};
use crate::events::{OnModeSwitch, PostInsertChar};
use crate::handlers::Handlers;
use crate::runtime::{send_task_event_with, RuntimeEvent, RuntimeTaskEvent};
use crate::ui::lsp::signature_help::SignatureHelp;

#[derive(Debug, PartialEq, Eq)]
enum State {
    Open,
    Closed,
    Pending,
}

/// debounce timeout in ms, value taken from VSCode
/// TODO: make this configurable?
const TIMEOUT: u64 = 120;

#[derive(Debug)]
pub(super) struct SignatureHelpHandler {
    trigger: Option<SignatureHelpInvoked>,
    state: State,
    request: Option<SignatureHelpRequestId>,
    cancel: Option<Token>,
    next_request: u64,
    debounce: Debounce,
    work: Work,
    clock: Clock,
    ingress: helix_runtime::Sender<RuntimeEvent>,
}

impl SignatureHelpHandler {
    pub fn new(
        work: Work,
        clock: Clock,
        ingress: helix_runtime::Sender<RuntimeEvent>,
    ) -> SignatureHelpHandler {
        SignatureHelpHandler {
            trigger: None,
            state: State::Closed,
            request: None,
            cancel: None,
            next_request: 1,
            debounce: Debounce::new(Duration::from_millis(TIMEOUT)),
            work,
            clock,
            ingress,
        }
    }

    fn next_request(&mut self) -> SignatureHelpRequestId {
        let id = std::num::NonZeroU64::new(self.next_request).expect("non-zero request id");
        self.next_request += 1;
        SignatureHelpRequestId::new(id)
    }

    fn dispatch_request(&mut self, invocation: SignatureHelpInvoked, delay: bool) {
        let request = self.next_request();
        let cancel = Token::new();
        if let Some(current) = self.cancel.replace(cancel.clone()) {
            current.cancel();
        }
        self.request = Some(request);
        self.state = State::Pending;
        let ingress = self.ingress.clone();

        let send = async move {
            send_task_event_with(
                RuntimeTaskEvent::RequestSignatureDebounced {
                    invoked: invocation,
                    request,
                    cancel,
                },
                ingress,
            )
            .await;
        };

        if delay {
            self.debounce.restart(&self.work, &self.clock, send);
        } else {
            self.debounce.cancel();
            self.work.spawn(send).detach();
        }
    }

    fn event(&mut self, event: SignatureHelpEvent) {
        match event {
            SignatureHelpEvent::Invoked => {
                self.trigger = Some(SignatureHelpInvoked::Manual);
                self.state = State::Closed;
                self.dispatch_request(SignatureHelpInvoked::Manual, false);
                return;
            }
            SignatureHelpEvent::Trigger => {}
            SignatureHelpEvent::ReTrigger => {
                // don't retrigger if we aren't open/pending yet
                if matches!(self.state, State::Closed) {
                    return;
                }
            }
            SignatureHelpEvent::Cancel => {
                self.state = State::Closed;
                self.debounce.cancel();
                if let Some(cancel) = self.cancel.take() {
                    cancel.cancel();
                }
                self.request = None;
                return;
            }
            SignatureHelpEvent::RequestComplete { request, open } => {
                if self.request != Some(request) {
                    return;
                }
                self.state = if open { State::Open } else { State::Closed };
                self.cancel = None;
                self.request = None;
                return;
            }
        }
        if self.trigger.is_none() {
            self.trigger = Some(SignatureHelpInvoked::Automatic)
        }
        let invocation = self.trigger.take().unwrap();
        self.dispatch_request(invocation, true);
    }

    pub fn spawn(
        runtime: Runtime,
        ingress: helix_runtime::Sender<RuntimeEvent>,
    ) -> helix_runtime::Sender<SignatureHelpEvent> {
        let (tx, mut rx) = helix_runtime::channel(128);
        let work = runtime.work().clone();
        let clock = runtime.clock().clone();
        work.clone().spawn(async move {
            let mut handler = SignatureHelpHandler::new(work, clock, ingress);
            while let Some(event) = rx.recv().await {
                handler.event(event);
            }
        }).detach();
        tx
    }
}

fn signature_help_post_insert_char_hook(
    tx: &helix_runtime::Sender<SignatureHelpEvent>,
    PostInsertChar { cx, .. }: &mut PostInsertChar<'_, '_>,
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
            send_blocking(tx, SignatureHelpEvent::Trigger)
        }
    }
    Ok(())
}

pub(super) fn register_hooks(handlers: &Handlers) {
    let tx = handlers.signature_hints.clone();
    register_hook!(move |event: &mut OnModeSwitch<'_, '_>| {
        match (event.old_mode, event.new_mode) {
            (Mode::Insert, _) => {
                send_blocking(&tx, SignatureHelpEvent::Cancel);
                event
                    .cx
                    .callback
                    .push(crate::compositor::PostAction::RemoveById(SignatureHelp::ID));
            }
            (_, Mode::Insert) if event.cx.editor.config().lsp.auto_signature_help => {
                send_blocking(&tx, SignatureHelpEvent::Trigger);
            }
            _ => (),
        }
        Ok(())
    });

    let tx = handlers.signature_hints.clone();
    register_hook!(
        move |event: &mut PostInsertChar<'_, '_>| signature_help_post_insert_char_hook(&tx, event)
    );

    let tx = handlers.signature_hints.clone();
    register_hook!(move |event: &mut DocumentDidChange<'_>| {
        let hook_start = std::time::Instant::now();
        if event.doc.config.load().lsp.auto_signature_help && !event.ghost_transaction {
            send_blocking(&tx, SignatureHelpEvent::ReTrigger);
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

    let tx = handlers.signature_hints.clone();
    register_hook!(move |event: &mut SelectionDidChange<'_>| {
        if event.doc.config.load().lsp.auto_signature_help {
            send_blocking(&tx, SignatureHelpEvent::ReTrigger);
        }
        Ok(())
    });
}
