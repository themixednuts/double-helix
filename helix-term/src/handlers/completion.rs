use helix_runtime::Token;
use tokio::task::JoinSet;

use crate::runtime::send_ui_command_with;
use crate::runtime::ui::{CompletionCommand, UiCommand};

pub use crate::ui::completion_ingress::trigger_auto_completion;
pub use item::{CompletionItem, CompletionItems, CompletionResponse, LspCompletionItem};
pub(crate) use path::path_completion;
pub use request::CompletionHandler;
pub use request::Trigger;
pub(crate) use request::{request_completions_from_language_server, TriggerKind};
pub use resolve::{ResolveHandler, ResolveRuntime};
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
            UiCommand::Completion(Box::new(CompletionCommand::ApplyProviderResponse {
                request,
                response,
                is_incomplete,
            })),
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
