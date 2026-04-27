use helix_runtime::send_blocking;
use helix_view::document::Mode;
use helix_view::handlers::completion::CompletionEvent;
use helix_view::handlers::lsp::SignatureHelpEvent;
use helix_view::handlers::{AutoReloadEvent, AutoSaveEvent};

use crate::commands;
use crate::handlers::completion::trigger_auto_completion;
use crate::ui::lsp::signature_help::SignatureHelp;

#[derive(Clone)]
pub struct Notifier {
    pub ingress: helix_runtime::Sender<crate::runtime::RuntimeEvent>,
    pub plugin_events: helix_runtime::Sender<helix_plugin::PluginNotification>,
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
        send_blocking(
            &self.plugin_events,
            helix_plugin::PluginNotification::ModeChange { old_mode, new_mode },
        );
        send_blocking(&self.ingress, crate::runtime::RuntimeEvent::Redraw);
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
        event
            .cx
            .editor
            .send_completion_event(CompletionEvent::Cancel);
        clear_completions(event.cx);
        send_blocking(
            event.cx.editor.auto_save_sender(),
            AutoSaveEvent::LeftInsertMode,
        );
        send_blocking(
            event.cx.editor.auto_reload_sender(),
            AutoReloadEvent::LeftInsertMode,
        );
        send_blocking(
            event.cx.editor.signature_help_sender(),
            SignatureHelpEvent::Cancel,
        );
        event
            .cx
            .callback
            .push(crate::compositor::PostAction::RemoveById(SignatureHelp::ID));
    } else if event.new_mode == Mode::Insert {
        trigger_auto_completion(event.cx.editor, false);
        if event.cx.editor.config().lsp.auto_signature_help {
            send_blocking(
                event.cx.editor.signature_help_sender(),
                SignatureHelpEvent::Trigger,
            );
        }
    }

    for (view, _) in event.cx.editor.tree.views_mut() {
        view.diagnostics_handler.active = event.new_mode != Mode::Insert;
    }
}
