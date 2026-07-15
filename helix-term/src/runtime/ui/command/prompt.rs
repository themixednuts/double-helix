use crate::ui::prompt::completion::PromptCompletionPayload;

#[derive(Clone, Debug)]
pub struct PromptCompletionResult(pub(crate) PromptCompletionPayload);

/// Prompt-local work completed away from the input/render thread.
pub enum PromptCommand {
    CompletionReady(PromptCompletionResult),
}

impl std::fmt::Debug for PromptCommand {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CompletionReady(result) => formatter
                .debug_struct("CompletionReady")
                .field("prompt_id", &result.0.prompt_id)
                .field("generation", &result.0.generation)
                .field("completions", &result.0.completions.len())
                .finish(),
        }
    }
}
