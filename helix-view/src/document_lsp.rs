use std::collections::HashSet;

use crate::ViewId;
use helix_core::syntax;
use helix_core::text_annotations::InlineAnnotation;
use helix_core::{Assoc, ChangeSet, Range};
use helix_lsp::{lsp, LanguageServerId, OffsetEncoding};
use helix_runtime::Token;

#[derive(Debug, Clone, Default)]
pub struct DocumentColorSwatches {
    pub color_swatches: Vec<InlineAnnotation>,
    pub colors: Vec<syntax::Highlight>,
    pub color_swatches_padding: Vec<InlineAnnotation>,
}

#[derive(Debug, Clone)]
pub struct DocumentCodeLens {
    pub server_id: LanguageServerId,
    pub range: Range,
    pub offset_encoding: OffsetEncoding,
    pub lens: lsp::CodeLens,
    pub resolved: bool,
}

impl DocumentCodeLens {
    pub fn title(&self) -> Option<&str> {
        self.lens
            .command
            .as_ref()
            .map(|command| command.title.as_str())
    }
}

#[derive(Debug, Clone, Default)]
pub struct DocumentCodeLenses {
    pub lenses: Vec<DocumentCodeLens>,
}

impl DocumentCodeLenses {
    pub fn sorted(mut lenses: Vec<DocumentCodeLens>) -> Self {
        lenses.sort_by_key(|lens| (lens.range.from(), lens.range.to(), lens.server_id));
        Self { lenses }
    }
}

#[derive(Debug, Clone)]
pub struct DocumentLink {
    pub server_id: LanguageServerId,
    pub range: Range,
    pub offset_encoding: OffsetEncoding,
    pub link: lsp::DocumentLink,
    pub resolved: bool,
}

#[derive(Debug, Clone, Default)]
pub struct DocumentLinks {
    pub links: Vec<DocumentLink>,
}

impl DocumentLinks {
    pub fn sorted(mut links: Vec<DocumentLink>) -> Self {
        links.sort_by_key(|link| (link.range.from(), link.range.to(), link.server_id));
        Self { links }
    }
}

#[derive(Debug, Default)]
pub struct DocumentLspState {
    previous_diagnostic_id: Option<String>,
    color_swatches: Option<DocumentColorSwatches>,
    code_lenses: Option<DocumentCodeLenses>,
    document_links: Option<DocumentLinks>,
    color_swatch_cancel: Option<Token>,
    code_lens_cancel: Option<Token>,
    document_link_cancel: Option<Token>,
    folding_range_cancel: Option<Token>,
    lsp_fold_views: HashSet<ViewId>,
    pull_diagnostic_cancel: Option<Token>,
}

impl DocumentLspState {
    pub fn restart_pull_diagnostics(&mut self) -> Token {
        self.cancel_pull_diagnostics();
        let token = Token::new();
        self.pull_diagnostic_cancel = Some(token.clone());
        token
    }

    pub fn cancel_pull_diagnostics(&mut self) -> bool {
        let Some(token) = self.pull_diagnostic_cancel.take() else {
            return false;
        };
        let was_active = !token.is_canceled();
        token.cancel();
        was_active
    }

    pub fn previous_diagnostic_id(&self) -> Option<&str> {
        self.previous_diagnostic_id.as_deref()
    }

    pub fn set_previous_diagnostic_id(&mut self, previous_diagnostic_id: Option<String>) {
        self.previous_diagnostic_id = previous_diagnostic_id;
    }

    pub fn restart_color_swatches(&mut self) -> Token {
        self.cancel_color_swatches();
        let token = Token::new();
        self.color_swatch_cancel = Some(token.clone());
        token
    }

    pub fn cancel_color_swatches(&mut self) -> bool {
        let Some(token) = self.color_swatch_cancel.take() else {
            return false;
        };
        let was_active = !token.is_canceled();
        token.cancel();
        was_active
    }

    pub fn color_swatches(&self) -> Option<&DocumentColorSwatches> {
        self.color_swatches.as_ref()
    }

    pub fn clear_color_swatches(&mut self) {
        self.color_swatches = None;
    }

    pub fn set_color_swatches(&mut self, color_swatches: DocumentColorSwatches) {
        self.color_swatches = Some(color_swatches);
    }

    pub fn update_color_swatches(&mut self, changes: &ChangeSet) {
        let apply_color_swatch_changes = |annotations: &mut Vec<InlineAnnotation>| {
            changes.update_positions(
                annotations
                    .iter_mut()
                    .map(|annotation| (&mut annotation.char_idx, Assoc::After)),
            );
        };

        if let Some(DocumentColorSwatches {
            color_swatches,
            colors: _,
            color_swatches_padding,
        }) = &mut self.color_swatches
        {
            apply_color_swatch_changes(color_swatches);
            apply_color_swatch_changes(color_swatches_padding);
        }
    }

    pub fn restart_code_lenses(&mut self) -> Token {
        self.cancel_code_lenses();
        let token = Token::new();
        self.code_lens_cancel = Some(token.clone());
        token
    }

    pub fn cancel_code_lenses(&mut self) -> bool {
        let Some(token) = self.code_lens_cancel.take() else {
            return false;
        };
        let was_active = !token.is_canceled();
        token.cancel();
        was_active
    }

    pub fn code_lenses(&self) -> Option<&DocumentCodeLenses> {
        self.code_lenses.as_ref()
    }

    pub fn code_lenses_mut(&mut self) -> Option<&mut DocumentCodeLenses> {
        self.code_lenses.as_mut()
    }

    pub fn clear_code_lenses(&mut self) {
        self.code_lenses = None;
    }

    pub fn set_code_lenses(&mut self, code_lenses: DocumentCodeLenses) {
        self.code_lenses = Some(code_lenses);
    }

    pub fn update_code_lenses(&mut self, changes: &ChangeSet) {
        if let Some(code_lenses) = &mut self.code_lenses {
            for lens in &mut code_lenses.lenses {
                changes.update_positions(
                    [
                        (&mut lens.range.anchor, Assoc::After),
                        (&mut lens.range.head, Assoc::Before),
                    ]
                    .into_iter(),
                );
            }
        }
    }

    pub fn restart_document_links(&mut self) -> Token {
        self.cancel_document_links();
        let token = Token::new();
        self.document_link_cancel = Some(token.clone());
        token
    }

    pub fn cancel_document_links(&mut self) -> bool {
        let Some(token) = self.document_link_cancel.take() else {
            return false;
        };
        let was_active = !token.is_canceled();
        token.cancel();
        was_active
    }

    pub fn document_links(&self) -> Option<&DocumentLinks> {
        self.document_links.as_ref()
    }

    pub fn clear_document_links(&mut self) {
        self.document_links = None;
    }

    pub fn set_document_links(&mut self, document_links: DocumentLinks) {
        self.document_links = Some(document_links);
    }

    pub fn update_document_links(&mut self, changes: &ChangeSet) {
        if let Some(document_links) = &mut self.document_links {
            for link in &mut document_links.links {
                changes.update_positions(
                    [
                        (&mut link.range.anchor, Assoc::After),
                        (&mut link.range.head, Assoc::Before),
                    ]
                    .into_iter(),
                );
            }
        }
    }

    pub fn restart_folding_ranges(&mut self) -> Token {
        self.cancel_folding_ranges();
        let token = Token::new();
        self.folding_range_cancel = Some(token.clone());
        token
    }

    pub fn cancel_folding_ranges(&mut self) -> bool {
        let Some(token) = self.folding_range_cancel.take() else {
            return false;
        };
        let was_active = !token.is_canceled();
        token.cancel();
        was_active
    }

    pub fn mark_lsp_fold_container(&mut self, view_id: ViewId) {
        self.lsp_fold_views.insert(view_id);
    }

    pub fn clear_lsp_fold_container(&mut self, view_id: ViewId) {
        self.lsp_fold_views.remove(&view_id);
    }

    pub fn is_lsp_fold_container(&self, view_id: ViewId) -> bool {
        self.lsp_fold_views.contains(&view_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lsp_range(line: u32, start: u32, end: u32) -> lsp::Range {
        lsp::Range::new(
            lsp::Position::new(line, start),
            lsp::Position::new(line, end),
        )
    }

    #[test]
    fn code_lenses_are_sorted_by_document_range() {
        let lenses = DocumentCodeLenses::sorted(vec![
            DocumentCodeLens {
                server_id: LanguageServerId::default(),
                range: Range::new(10, 12),
                offset_encoding: OffsetEncoding::Utf8,
                lens: lsp::CodeLens {
                    range: lsp_range(1, 0, 1),
                    command: None,
                    data: None,
                },
                resolved: false,
            },
            DocumentCodeLens {
                server_id: LanguageServerId::default(),
                range: Range::new(3, 5),
                offset_encoding: OffsetEncoding::Utf8,
                lens: lsp::CodeLens {
                    range: lsp_range(0, 0, 1),
                    command: None,
                    data: None,
                },
                resolved: false,
            },
        ]);

        assert_eq!(
            lenses
                .lenses
                .iter()
                .map(|lens| lens.range.from())
                .collect::<Vec<_>>(),
            vec![3, 10]
        );
    }

    #[test]
    fn document_links_are_sorted_by_document_range() {
        let links = DocumentLinks::sorted(vec![
            DocumentLink {
                server_id: LanguageServerId::default(),
                range: Range::new(8, 12),
                offset_encoding: OffsetEncoding::Utf8,
                link: lsp::DocumentLink {
                    range: lsp_range(1, 0, 4),
                    target: None,
                    tooltip: None,
                    data: None,
                },
                resolved: false,
            },
            DocumentLink {
                server_id: LanguageServerId::default(),
                range: Range::new(1, 4),
                offset_encoding: OffsetEncoding::Utf8,
                link: lsp::DocumentLink {
                    range: lsp_range(0, 0, 3),
                    target: None,
                    tooltip: None,
                    data: None,
                },
                resolved: false,
            },
        ]);

        assert_eq!(
            links
                .links
                .iter()
                .map(|link| link.range.from())
                .collect::<Vec<_>>(),
            vec![1, 8]
        );
    }
}
