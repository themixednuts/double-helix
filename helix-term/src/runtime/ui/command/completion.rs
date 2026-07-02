use std::{collections::HashMap, sync::Arc};

use crate::handlers::completion::{CompletionItem, CompletionResponse, LspCompletionItem, Trigger};
use helix_core::completion::CompletionProvider;
use helix_view::handlers::completion::{RequestId as CompletionRequestId, ResponseContext};

/// Completion-specific UI ingress (async completion list / resolve).
pub enum CompletionCommand {
    ApplyProviderResponse {
        request: CompletionRequestId,
        response: CompletionResponse,
        is_incomplete: bool,
    },
    ReplaceResolvedItem {
        previous: Arc<LspCompletionItem>,
        resolved: Box<CompletionItem>,
    },
    Show {
        request: CompletionRequestId,
        items: Vec<CompletionItem>,
        context: HashMap<CompletionProvider, ResponseContext>,
        trigger: Trigger,
    },
    /// Debounced completion request after [`CompletionHandler`] timeout.
    RequestDebounced { trigger: Trigger },
}

impl std::fmt::Debug for CompletionCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ApplyProviderResponse { .. } => f.write_str("ApplyProviderResponse(..)"),
            Self::ReplaceResolvedItem { .. } => f.write_str("ReplaceResolvedItem(..)"),
            Self::Show { .. } => f.write_str("Show(..)"),
            Self::RequestDebounced { .. } => f.write_str("RequestDebounced(..)"),
        }
    }
}
