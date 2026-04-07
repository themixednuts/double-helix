//! Main-thread completion UI updates (ingress path). Used by [`crate::runtime::ui::apply`]
//! instead of legacy callback ingress so `runtime::ui` does not depend on `handlers` for apply.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use helix_core::chars::char_is_word;
use helix_core::completion::CompletionProvider;
use helix_core::syntax::config::LanguageServerFeature;
use helix_lsp::lsp;
use helix_lsp::lsp::{CompletionContext, CompletionTriggerKind};
use helix_stdx::rope::RopeSliceExt;
use helix_view::document::{Mode, SavePoint};
use helix_view::handlers::completion::{CompletionEvent, RequestId, ResponseContext};
use helix_view::Editor;
use tokio::task::JoinSet;
use tokio::time::{timeout_at, Instant};

use crate::compositor::Compositor;
use crate::handlers::completion::{
    handle_response, replace_completions, request_completions_from_language_server,
    retain_valid_completions_for_trigger, word_completion, CompletionItem,
    CompletionResponse, LspCompletionItem, Trigger, TriggerKind,
};
use crate::handlers::completion::path_completion;
use crate::runtime::ui::{CompletionCommand, UiCommand};
use crate::runtime::{send_ui_command_with, RuntimeEvent};
use crate::ui::lsp::signature_help::SignatureHelp;
use crate::ui::{self, Popup};

/// Apply incremental LSP completion list updates while the completion menu is open.
pub(crate) fn apply_provider_completion_response(
    editor: &mut Editor,
    compositor: &mut Compositor,
    request: RequestId,
    mut response: CompletionResponse,
    is_incomplete: bool,
) {
    let editor_view = compositor.find::<ui::EditorView>().unwrap();
    let Some(completion) = &mut editor_view.completion else {
        return;
    };
    if !editor.handlers.completions.is_current(request) {
        log::info!("dropping outdated completion response");
        return;
    }

    completion.replace_provider_completions(&mut response, is_incomplete);
    if completion.is_empty() {
        editor_view.clear_completion(editor);
        trigger_auto_completion(editor, false);
    } else {
        editor
            .handlers
            .completions
            .active_completions
            .insert(response.provider, response.context);
    }
}

/// Replace a completion item after `completionItem/resolve`.
pub(crate) fn apply_resolved_completion_item(
    compositor: &mut Compositor,
    previous: &LspCompletionItem,
    resolved: CompletionItem,
) {
    if let Some(completion) = &mut compositor.find::<ui::EditorView>().unwrap().completion {
        completion.replace_item(previous, resolved);
    }
}

/// Initial completion popup (aggregated list after the first response batch).
pub(crate) fn show_completion_popup(
    editor: &mut Editor,
    compositor: &mut Compositor,
    ingress: helix_runtime::Sender<crate::runtime::RuntimeEvent>,
    request: RequestId,
    mut items: Vec<CompletionItem>,
    context: HashMap<CompletionProvider, ResponseContext>,
    trigger: Trigger,
) {
    if !editor.handlers.completions.is_current(request) {
        return;
    }
    let (view_id, doc) = focused_ref!(editor);
    if editor.mode != Mode::Insert || view_id != trigger.view || doc.id() != trigger.doc {
        return;
    }

    let size = compositor.size();
    let ui = compositor.find::<ui::EditorView>().unwrap();
    if ui.completion.is_some() {
        return;
    }
    retain_valid_completions_for_trigger(trigger, doc, view_id, &mut items);
    editor.handlers.completions.active_completions = context;

    let completion_area = ui.set_completion(editor, items, trigger.pos, size, ingress);
    let signature_help_area = compositor
        .find_id::<Popup<SignatureHelp>>(SignatureHelp::ID)
        .map(|signature_help| signature_help.area(size, editor));
    if matches!((completion_area, signature_help_area), (Some(a), Some(b)) if a.intersects(b)) {
        compositor.remove(SignatureHelp::ID);
    }
}

pub(crate) fn request_completions(
    mut trigger: Trigger,
    editor: &mut Editor,
    compositor: &mut Compositor,
    ingress: helix_runtime::Sender<RuntimeEvent>,
) {
    let (view_id, doc) = focused_ref!(editor);

    if compositor
        .find::<ui::EditorView>()
        .unwrap()
        .completion
        .is_some()
        || editor.mode != Mode::Insert
    {
        return;
    }

    let text = doc.text();
    let cursor = doc.selection(view_id).primary().cursor(text.slice(..));
    if trigger.view != view_id || trigger.doc != doc.id() || cursor < trigger.pos {
        return;
    }
    trigger.pos = cursor;
    let (request, cancel) = editor.handlers.completions.begin_request();
    let view = view!(editor, view_id);
    let doc = doc_mut!(editor, &doc.id());
    let savepoint = doc.savepoint(view);
    let text = doc.text();
    let trigger_text = text.slice(..cursor);

    let mut seen_language_servers = HashSet::new();
    let language_servers: Vec<_> = doc
        .language_servers_with_feature(LanguageServerFeature::Completion)
        .filter(|ls| seen_language_servers.insert(ls.id()))
        .collect();
    let mut requests = JoinSet::new();
    for (priority, ls) in language_servers.iter().enumerate() {
        let context = if trigger.kind == TriggerKind::Manual {
            lsp::CompletionContext {
                trigger_kind: lsp::CompletionTriggerKind::INVOKED,
                trigger_character: None,
            }
        } else {
            let trigger_char = ls
                .capabilities()
                .completion_provider
                .as_ref()
                .and_then(|provider| {
                    provider
                        .trigger_characters
                        .as_deref()?
                        .iter()
                        .find(|&trigger_char| trigger_text.ends_with(trigger_char))
                });

            if trigger_char.is_some() {
                lsp::CompletionContext {
                    trigger_kind: lsp::CompletionTriggerKind::TRIGGER_CHARACTER,
                    trigger_character: trigger_char.cloned(),
                }
            } else {
                lsp::CompletionContext {
                    trigger_kind: lsp::CompletionTriggerKind::INVOKED,
                    trigger_character: None,
                }
            }
        };
        requests.spawn(request_completions_from_language_server(
            ls,
            doc,
            view_id,
            context,
            -(priority as i8),
            savepoint.clone(),
        ));
    }
    if let Some(path_completion_request) = path_completion(
        doc.selection(view_id).clone(),
        doc,
        cancel.clone(),
        savepoint.clone(),
    ) {
        requests.spawn_blocking(path_completion_request);
    }
    if let Some(word_completion_request) =
        word_completion(editor, trigger, cancel.clone(), savepoint)
    {
        requests.spawn_blocking(word_completion_request);
    }

    let replace_cancel = cancel.clone();
    let request_cancel = cancel.clone();
    let request_completions = async move {
        if request_cancel.is_canceled() {
            return;
        }
        let mut context = HashMap::new();
        let Some(mut response) = handle_response(&mut requests, false).await else {
            return;
        };

        let mut items: Vec<_> = Vec::new();
        response.take_items(&mut items);
        context.insert(response.provider, response.context);
        let deadline = Instant::now() + Duration::from_millis(100);
        loop {
            let Some(mut response) = timeout_at(deadline, handle_response(&mut requests, false))
                .await
                .ok()
                .flatten()
            else {
                break;
            };
            response.take_items(&mut items);
            context.insert(response.provider, response.context);
        }
        send_ui_command_with(
            UiCommand::Completion(CompletionCommand::Show {
                request,
                items,
                context,
                trigger,
            }),
            ingress.clone(),
        )
        .await;
        if !requests.is_empty() {
            replace_completions(request, replace_cancel, requests, false, ingress).await;
        }
    };
    let wait_cancel = cancel.clone();
    editor.runtime().work().clone().spawn(async move {
        tokio::select! {
            _ = wait_cancel.canceled() => {}
            _ = request_completions => {}
        }
    }).detach();
}

pub(crate) fn request_incomplete_completion_list(
    editor: &mut Editor,
    ingress: helix_runtime::Sender<RuntimeEvent>,
) {
    let handler = &mut editor.handlers.completions;
    let (request, cancel) = handler.begin_request();
    let mut requests = JoinSet::new();
    let mut savepoint: Option<Arc<SavePoint>> = None;
    for (&provider, context) in &handler.active_completions {
        if !context.is_incomplete {
            continue;
        }
        let CompletionProvider::Lsp(ls_id) = provider else {
            log::error!("non-lsp incomplete completion lists");
            continue;
        };
        let Some(ls) = editor.language_servers.get_by_id(ls_id) else {
            continue;
        };
        let (view_id, doc) = focused!(editor);
        let view = view!(editor, view_id);
        let savepoint = savepoint.get_or_insert_with(|| doc.savepoint(view)).clone();
        let request = request_completions_from_language_server(
            ls,
            doc,
            view_id,
            CompletionContext {
                trigger_kind: CompletionTriggerKind::TRIGGER_FOR_INCOMPLETE_COMPLETIONS,
                trigger_character: None,
            },
            context.priority,
            savepoint,
        );
        requests.spawn(request);
    }
    if !requests.is_empty() {
        editor
            .runtime()
            .work()
            .clone()
            .spawn(replace_completions(request, cancel, requests, true, ingress))
            .detach();
    }
}

/// Re-trigger completion after apply/clear (same behavior as the legacy handler path).
pub fn trigger_auto_completion(editor: &Editor, trigger_char_only: bool) {
    let config = editor.config.load();
    if !config.auto_completion {
        return;
    }
    let (view_id, doc) = focused_ref!(editor);
    let mut text = doc.text().slice(..);
    let cursor = doc.selection(view_id).primary().cursor(text);
    text = doc.text().slice(..cursor);

    let is_trigger_char = doc
        .language_servers_with_feature(LanguageServerFeature::Completion)
        .any(|ls| {
            matches!(&ls.capabilities().completion_provider, Some(lsp::CompletionOptions {
                        trigger_characters: Some(triggers),
                        ..
                    }) if triggers.iter().any(|trigger| text.ends_with(trigger)))
        });

    let cursor_char = text
        .get_bytes_at(text.len_bytes())
        .and_then(|t| t.reversed().next());

    #[cfg(windows)]
    let is_path_completion_trigger = matches!(cursor_char, Some(b'/' | b'\\'));
    #[cfg(not(windows))]
    let is_path_completion_trigger = matches!(cursor_char, Some(b'/'));

    let handler = &editor.handlers.completions;
    if is_trigger_char || (is_path_completion_trigger && doc.path_completion_enabled()) {
        handler.event(CompletionEvent::TriggerChar {
            cursor,
            doc: doc.id(),
            view: view_id,
        });
        return;
    }

    let is_auto_trigger = !trigger_char_only
        && doc
            .text()
            .chars_at(cursor)
            .reversed()
            .take(config.completion_trigger_len as usize)
            .all(char_is_word);

    if is_auto_trigger {
        handler.event(CompletionEvent::AutoTrigger {
            cursor,
            doc: doc.id(),
            view: view_id,
        });
    }
}
