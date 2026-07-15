use helix_view::document::Mode;
use helix_view::handlers::completion::CompletionEvent;
use helix_view::handlers::lsp::SignatureHelpEvent;
use helix_view::handlers::{AutoReloadEvent, AutoSaveEvent};

use crate::commands;
use crate::handlers::completion::trigger_auto_completion;
use crate::ui::lsp::signature_help::SignatureHelp;

#[derive(Clone)]
pub struct Notifier {
    pub redraw: helix_runtime::FrameHandle,
    pub plugin_events: PluginEventSender,
}

#[derive(Clone)]
pub enum PluginEventSender {
    Ingress(crate::runtime::RuntimeIngress),
    Mailbox(helix_runtime::Sender<crate::runtime::PluginNotification>),
}

impl PluginEventSender {
    pub fn notify(&self, notification: crate::runtime::PluginNotification) {
        match self {
            Self::Ingress(ingress) => {
                if let Err(error) = ingress.plugin(notification) {
                    log::warn!("plugin notification admission failed: {error}");
                }
            }
            Self::Mailbox(sender) => {
                if let Err(error) = sender.try_send(notification) {
                    log::debug!("embedded plugin notification dropped: {error}");
                }
            }
        }
    }
}

impl From<crate::runtime::RuntimeIngress> for PluginEventSender {
    fn from(ingress: crate::runtime::RuntimeIngress) -> Self {
        Self::Ingress(ingress)
    }
}

impl From<helix_runtime::Sender<crate::runtime::PluginNotification>> for PluginEventSender {
    fn from(sender: helix_runtime::Sender<crate::runtime::PluginNotification>) -> Self {
        Self::Mailbox(sender)
    }
}

pub struct ModeSwitch<'a, 'cx> {
    pub old_mode: Mode,
    pub new_mode: Mode,
    pub cx: &'a mut commands::Context<'cx>,
}

impl Notifier {
    pub fn mode_switch(&self, old_mode: Mode, new_mode: Mode) {
        let old_mode = format!("{old_mode:?}");
        let new_mode = format!("{new_mode:?}");
        self.plugin_events
            .notify(crate::runtime::PluginNotification::ModeChange { old_mode, new_mode });
        self.redraw.request_redraw();
    }
}

fn update_completion_filter(cx: &mut commands::Context, c: Option<char>) {
    cx.callback
        .push(crate::compositor::PostAction::UpdateCompletionFilter(c))
}

fn clear_completions(cx: &mut commands::Context) {
    cx.callback
        .push(crate::compositor::PostAction::ClearCompletion)
}

pub fn post_command(command: &'static crate::keymap::MappableCommand, cx: &mut commands::Context) {
    if cx.editor.mode != Mode::Insert {
        return;
    }

    if !matches!(
        command.name(),
        "inline_completion" | "accept_inline_completion"
    ) {
        focused!(cx.editor).1.clear_inline_completion();
    }

    if cx.editor.last_completion.is_some() {
        match command {
            crate::keymap::MappableCommand::Engine { .. }
                if matches!(
                    command.name(),
                    "delete_word_forward" | "delete_char_forward"
                ) => {}
            crate::keymap::MappableCommand::Frontend { .. } if command.name() == "completion" => {}
            crate::keymap::MappableCommand::Engine { .. }
                if command.name() == "delete_char_backward" =>
            {
                update_completion_filter(cx, None)
            }
            _ => clear_completions(cx),
        }
        return;
    }

    let event = match command {
        crate::keymap::MappableCommand::Engine { .. }
            if matches!(
                command.name(),
                "delete_char_backward" | "delete_word_forward" | "delete_char_forward"
            ) =>
        {
            let (view_id, doc) = focused!(cx.editor);
            let primary_cursor = doc
                .selection(view_id)
                .primary()
                .cursor(doc.text().slice(..));
            CompletionEvent::DeleteText {
                cursor: primary_cursor,
            }
        }
        crate::keymap::MappableCommand::Frontend { .. }
            if matches!(command.name(), "completion" | "insert_mode" | "append_mode") =>
        {
            return;
        }
        _ => CompletionEvent::Cancel,
    };
    cx.editor.send_completion_event(event);
}

pub fn post_insert_char(c: char, cx: &mut commands::Context) {
    if cx.editor.last_completion.is_some() {
        update_completion_filter(cx, Some(c));
    } else {
        trigger_auto_completion(cx.editor, false);
    }

    let signature_hints = cx.editor.signature_help_sender().clone();
    crate::handlers::signature_help::signature_help_post_insert_char_hook(&signature_hints, cx)
        .ok();
}

pub fn mode_switch(event: &mut ModeSwitch<'_, '_>) {
    if event.old_mode == Mode::Insert {
        focused!(event.cx.editor).1.clear_inline_completion();
        event
            .cx
            .editor
            .send_completion_event(CompletionEvent::Cancel);
        clear_completions(event.cx);
        event
            .cx
            .editor
            .auto_save_sender()
            .send(AutoSaveEvent::LeftInsertMode);
        event
            .cx
            .editor
            .auto_reload_sender()
            .send(AutoReloadEvent::LeftInsertMode);
        event
            .cx
            .editor
            .signature_help_sender()
            .send(SignatureHelpEvent::Cancel);
        event
            .cx
            .callback
            .push(crate::compositor::PostAction::RemoveById(SignatureHelp::ID));
    } else if event.new_mode == Mode::Insert {
        trigger_auto_completion(event.cx.editor, false);
        if event.cx.editor.config().lsp.auto_signature_help {
            event
                .cx
                .editor
                .signature_help_sender()
                .send(SignatureHelpEvent::Trigger);
        }
    }

    for (view, _) in event.cx.editor.tree.views_mut() {
        view.diagnostics_handler.active = event.new_mode != Mode::Insert;
    }
}
