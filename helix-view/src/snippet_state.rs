use helix_core::snippets::{ActiveSnippet, RenderedSnippet};

#[derive(Default)]
pub struct DocumentSnippetState {
    active_snippet: Option<ActiveSnippet>,
}

impl DocumentSnippetState {
    pub fn active_snippet(&self) -> Option<&ActiveSnippet> {
        self.active_snippet.as_ref()
    }

    pub fn active_snippet_mut(&mut self) -> Option<&mut ActiveSnippet> {
        self.active_snippet.as_mut()
    }

    pub fn has_active_snippet(&self) -> bool {
        self.active_snippet.is_some()
    }

    pub fn take_active_snippet(&mut self) -> Option<ActiveSnippet> {
        self.active_snippet.take()
    }

    pub fn set_active_snippet(&mut self, snippet: ActiveSnippet) {
        self.active_snippet = Some(snippet);
    }

    pub fn clear_active_snippet(&mut self) {
        self.active_snippet = None;
    }

    pub fn apply_rendered_snippet(&mut self, snippet: RenderedSnippet) {
        self.active_snippet = match self.active_snippet.take() {
            Some(active) => active.insert_subsnippet(snippet),
            None => ActiveSnippet::new(snippet),
        };
    }
}
