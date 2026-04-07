use helix_event::register_hook;
use helix_runtime::Token;
use helix_view::document::Mode;
use helix_view::handlers::completion::CompletionEvent;
use tokio::task::JoinSet;

use crate::commands;
use crate::events::{OnModeSwitch, PostCommand, PostInsertChar};
use crate::keymap::MappableCommand;
use crate::runtime::send_ui_command_with;
use crate::runtime::ui::{CompletionCommand, UiCommand};

use super::Handlers;

pub use crate::ui::completion_ingress::trigger_auto_completion;
pub use item::{CompletionItem, CompletionItems, CompletionResponse, LspCompletionItem};
pub(crate) use path::path_completion;
pub use request::CompletionHandler;
pub(crate) use request::{request_completions_from_language_server, TriggerKind};
pub use request::Trigger;
pub use resolve::ResolveHandler;
pub(crate) use word::completion as word_completion;

mod item;
mod path;
mod request;
mod resolve;
mod word;

pub(crate) async fn handle_response(
    requests: &mut JoinSet<CompletionResponse>,
    is_incomplete: bool,
) -> Option<CompletionResponse> {
    loop {
        let response = requests.join_next().await?.unwrap();
        if !is_incomplete && !response.context.is_incomplete && response.items.is_empty() {
            continue;
        }
        return Some(response);
    }
}

pub(crate) async fn replace_completions(
    request: helix_view::handlers::completion::RequestId,
    cancel: Token,
    mut requests: JoinSet<CompletionResponse>,
    is_incomplete: bool,
    ingress: helix_runtime::Sender<crate::runtime::RuntimeEvent>,
) {
    while let Some(response) = handle_response(&mut requests, is_incomplete).await {
        if cancel.is_canceled() {
            break;
        }
        send_ui_command_with(
            UiCommand::Completion(CompletionCommand::ApplyProviderResponse {
                request,
                response,
                is_incomplete,
            }),
            ingress.clone(),
        )
        .await;
    }
}

/// Used by [`crate::ui::completion_ingress`] for filtering; keeps `mod word` private to handlers.
pub(crate) fn retain_valid_completions_for_trigger(
    trigger: Trigger,
    doc: &helix_view::Document,
    view_id: helix_view::ViewId,
    items: &mut Vec<CompletionItem>,
) {
    word::retain_valid_completions(trigger, doc, view_id, items);
}

fn update_completion_filter(cx: &mut commands::Context, c: Option<char>) {
    cx.callback
        .push(crate::compositor::PostAction::UpdateCompletionFilter(c))
}

fn clear_completions(cx: &mut commands::Context) {
    cx.callback.push(crate::compositor::PostAction::ClearCompletion)
}

fn completion_post_command_hook(
    PostCommand { command, cx }: &mut PostCommand<'_, '_>,
) -> anyhow::Result<()> {
    if cx.editor.mode == Mode::Insert {
        if cx.editor.last_completion.is_some() {
            match command {
                MappableCommand::Engine { .. }
                    if matches!(
                        command.name(),
                        "delete_word_forward" | "delete_char_forward"
                    ) => {}
                MappableCommand::Frontend { .. } if command.name() == "completion" => (),
                MappableCommand::Engine { .. } if command.name() == "delete_char_backward" => {
                    update_completion_filter(cx, None)
                }
                _ => clear_completions(cx),
            }
        } else {
            let event = match command {
                MappableCommand::Engine { .. }
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
                // hacks: some commands are handeled elsewhere and we don't want to
                // cancel in that case
                MappableCommand::Frontend { .. }
                    if matches!(command.name(), "completion" | "insert_mode" | "append_mode") =>
                {
                    return Ok(());
                }
                _ => CompletionEvent::Cancel,
            };
            cx.editor.handlers.completions.event(event);
        }
    }
    Ok(())
}

pub(super) fn register_hooks(_handlers: &Handlers) {
    register_hook!(move |event: &mut PostCommand<'_, '_>| completion_post_command_hook(event));

    register_hook!(move |event: &mut OnModeSwitch<'_, '_>| {
        if event.old_mode == Mode::Insert {
            event
                .cx
                .editor
                .handlers
                .completions
                .event(CompletionEvent::Cancel);
            clear_completions(event.cx);
        } else if event.new_mode == Mode::Insert {
            trigger_auto_completion(event.cx.editor, false)
        }
        Ok(())
    });

    register_hook!(move |event: &mut PostInsertChar<'_, '_>| {
        if event.cx.editor.last_completion.is_some() {
            update_completion_filter(event.cx, Some(event.c))
        } else {
            trigger_auto_completion(event.cx.editor, false);
        }
        Ok(())
    });
}
