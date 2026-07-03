use std::collections::{HashMap, HashSet};

use crate::ViewId;
use helix_core::syntax::{self, OverlayHighlights};
use helix_core::text_annotations::InlineAnnotation;
use helix_core::{Assoc, ChangeSet, Range, Rope};
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentSemanticToken {
    pub range: Range,
    pub token_type: String,
    pub token_modifiers: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct DocumentSemanticTokens {
    pub version: i32,
    pub tokens: Vec<DocumentSemanticToken>,
}

#[derive(Debug, Clone)]
pub struct InlineCompletionGhost {
    pub view_id: ViewId,
    pub version: i32,
    pub cursor: usize,
    pub text: String,
    pub annotation: InlineAnnotation,
    pub replace_range: Option<Range>,
}

#[derive(Debug, Clone, Default)]
pub struct DocumentInlineValues {
    pub annotations: Vec<InlineAnnotation>,
}

#[derive(Debug, Default)]
pub struct DocumentLspState {
    previous_diagnostic_id: Option<String>,
    color_swatches: Option<DocumentColorSwatches>,
    code_lenses: Option<DocumentCodeLenses>,
    document_links: Option<DocumentLinks>,
    semantic_tokens: HashMap<LanguageServerId, DocumentSemanticTokens>,
    inline_completion: Option<InlineCompletionGhost>,
    inline_values: Option<DocumentInlineValues>,
    color_swatch_cancel: Option<Token>,
    code_lens_cancel: Option<Token>,
    document_link_cancel: Option<Token>,
    folding_range_cancel: Option<Token>,
    semantic_token_cancel: Option<Token>,
    inline_completion_cancel: Option<Token>,
    inline_value_cancel: Option<Token>,
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

    pub fn restart_semantic_tokens(&mut self) -> Token {
        self.cancel_semantic_tokens();
        let token = Token::new();
        self.semantic_token_cancel = Some(token.clone());
        token
    }

    pub fn cancel_semantic_tokens(&mut self) -> bool {
        let Some(token) = self.semantic_token_cancel.take() else {
            return false;
        };
        let was_active = !token.is_canceled();
        token.cancel();
        was_active
    }

    pub fn semantic_tokens(&self) -> &HashMap<LanguageServerId, DocumentSemanticTokens> {
        &self.semantic_tokens
    }

    pub fn clear_semantic_tokens(&mut self) {
        self.semantic_tokens.clear();
    }

    pub fn set_semantic_tokens(
        &mut self,
        server_id: LanguageServerId,
        tokens: DocumentSemanticTokens,
    ) {
        self.semantic_tokens.insert(server_id, tokens);
    }

    pub fn update_semantic_tokens(&mut self, changes: &ChangeSet) {
        self.semantic_tokens.clear();
        if let Some(ghost) = &mut self.inline_completion {
            changes.update_positions([(&mut ghost.cursor, Assoc::After)].into_iter());
            changes.update_positions([(&mut ghost.annotation.char_idx, Assoc::After)].into_iter());
            if let Some(range) = &mut ghost.replace_range {
                changes.update_positions(
                    [
                        (&mut range.anchor, Assoc::After),
                        (&mut range.head, Assoc::Before),
                    ]
                    .into_iter(),
                );
            }
        }
        if let Some(inline_values) = &mut self.inline_values {
            changes.update_positions(
                inline_values
                    .annotations
                    .iter_mut()
                    .map(|annotation| (&mut annotation.char_idx, Assoc::After)),
            );
        }
    }

    pub fn restart_inline_completion(&mut self) -> Token {
        self.cancel_inline_completion();
        let token = Token::new();
        self.inline_completion_cancel = Some(token.clone());
        token
    }

    pub fn cancel_inline_completion(&mut self) -> bool {
        let Some(token) = self.inline_completion_cancel.take() else {
            self.inline_completion = None;
            return false;
        };
        let was_active = !token.is_canceled();
        token.cancel();
        self.inline_completion = None;
        was_active
    }

    pub fn inline_completion(&self) -> Option<&InlineCompletionGhost> {
        self.inline_completion.as_ref()
    }

    pub fn set_inline_completion(&mut self, completion: InlineCompletionGhost) {
        self.inline_completion = Some(completion);
    }

    pub fn clear_inline_completion(&mut self) {
        self.inline_completion = None;
    }

    pub fn restart_inline_values(&mut self) -> Token {
        self.cancel_inline_values();
        let token = Token::new();
        self.inline_value_cancel = Some(token.clone());
        token
    }

    pub fn cancel_inline_values(&mut self) -> bool {
        let Some(token) = self.inline_value_cancel.take() else {
            return false;
        };
        let was_active = !token.is_canceled();
        token.cancel();
        was_active
    }

    pub fn inline_values(&self) -> Option<&DocumentInlineValues> {
        self.inline_values.as_ref()
    }

    pub fn set_inline_values(&mut self, values: DocumentInlineValues) {
        self.inline_values = Some(values);
    }

    pub fn clear_inline_values(&mut self) {
        self.inline_values = None;
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

pub fn decode_semantic_tokens(
    text: &Rope,
    legend: &lsp::SemanticTokensLegend,
    offset_encoding: OffsetEncoding,
    tokens: &[lsp::SemanticToken],
) -> Vec<DocumentSemanticToken> {
    let mut line = 0u32;
    let mut start = 0u32;
    let mut decoded = Vec::with_capacity(tokens.len());

    for token in tokens {
        line = line.saturating_add(token.delta_line);
        start = if token.delta_line == 0 {
            start.saturating_add(token.delta_start)
        } else {
            token.delta_start
        };

        let Some(token_type) = legend.token_types.get(token.token_type as usize) else {
            continue;
        };
        let start_pos = lsp::Position::new(line, start);
        let end_pos = lsp::Position::new(line, start.saturating_add(token.length));
        let Some(start_char) = helix_lsp::util::lsp_pos_to_pos(text, start_pos, offset_encoding)
        else {
            continue;
        };
        let Some(end_char) = helix_lsp::util::lsp_pos_to_pos(text, end_pos, offset_encoding) else {
            continue;
        };
        if start_char >= end_char {
            continue;
        }

        let token_modifiers = legend
            .token_modifiers
            .iter()
            .enumerate()
            .filter(|(idx, _)| {
                *idx < u32::BITS as usize && token.token_modifiers_bitset & (1u32 << *idx) != 0
            })
            .map(|(_, modifier)| modifier.as_str().to_owned())
            .collect();

        decoded.push(DocumentSemanticToken {
            range: Range::new(start_char, end_char),
            token_type: token_type.as_str().to_owned(),
            token_modifiers,
        });
    }

    decoded
}

fn semantic_fallback_scope(token_type: &str) -> Option<&'static str> {
    Some(match token_type {
        "namespace" => "namespace",
        "type" | "class" | "enum" | "interface" | "struct" | "typeParameter" => "type",
        "parameter" => "variable.parameter",
        "variable" => "variable",
        "property" | "enumMember" => "variable.other.member",
        "function" | "method" => "function",
        "macro" => "function.macro",
        "keyword" | "modifier" => "keyword",
        "comment" => "comment",
        "string" => "string",
        "number" => "constant.numeric",
        "regexp" => "string.regexp",
        "operator" => "operator",
        "decorator" => "attribute",
        _ => return None,
    })
}

pub fn semantic_highlight(
    theme: &crate::Theme,
    token_type: &str,
    modifiers: &[String],
) -> Option<syntax::Highlight> {
    for modifier in modifiers {
        let scope = format!("semantic.{token_type}.{modifier}");
        if let Some(highlight) = theme.find_highlight(&scope) {
            return Some(highlight);
        }
    }
    let scope = format!("semantic.{token_type}");
    theme.find_highlight(&scope).or_else(|| {
        semantic_fallback_scope(token_type).and_then(|scope| theme.find_highlight(scope))
    })
}

pub fn semantic_tokens_overlay(
    theme: &crate::Theme,
    tokens: &HashMap<LanguageServerId, DocumentSemanticTokens>,
    version: i32,
) -> Option<OverlayHighlights> {
    let mut highlights = tokens
        .values()
        .filter(|set| set.version == version)
        .flat_map(|set| {
            set.tokens.iter().filter_map(|token| {
                semantic_highlight(theme, &token.token_type, &token.token_modifiers)
                    .map(|highlight| (highlight, token.range.from()..token.range.to()))
            })
        })
        .collect::<Vec<_>>();
    highlights.sort_by_key(|(_, range)| (range.start, range.end));
    (!highlights.is_empty()).then_some(OverlayHighlights::Heterogenous { highlights })
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

    fn semantic_legend() -> lsp::SemanticTokensLegend {
        lsp::SemanticTokensLegend {
            token_types: vec![
                lsp::SemanticTokenType::VARIABLE,
                lsp::SemanticTokenType::FUNCTION,
            ],
            token_modifiers: vec![
                lsp::SemanticTokenModifier::DECLARATION,
                lsp::SemanticTokenModifier::STATIC,
            ],
        }
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

    #[test]
    fn semantic_tokens_decode_relative_stream() {
        let text = Rope::from("hello\nworld\n");
        let tokens = decode_semantic_tokens(
            &text,
            &semantic_legend(),
            OffsetEncoding::Utf8,
            &[
                lsp::SemanticToken {
                    delta_line: 0,
                    delta_start: 0,
                    length: 5,
                    token_type: 0,
                    token_modifiers_bitset: 0b01,
                },
                lsp::SemanticToken {
                    delta_line: 1,
                    delta_start: 0,
                    length: 5,
                    token_type: 1,
                    token_modifiers_bitset: 0b10,
                },
            ],
        );

        assert_eq!(
            tokens,
            vec![
                DocumentSemanticToken {
                    range: Range::new(0, 5),
                    token_type: "variable".to_string(),
                    token_modifiers: vec!["declaration".to_string()],
                },
                DocumentSemanticToken {
                    range: Range::new(6, 11),
                    token_type: "function".to_string(),
                    token_modifiers: vec!["static".to_string()],
                },
            ]
        );
    }

    #[test]
    fn semantic_tokens_decode_utf16_offsets() {
        let text = Rope::from("a😀b\n");
        let tokens = decode_semantic_tokens(
            &text,
            &semantic_legend(),
            OffsetEncoding::Utf16,
            &[lsp::SemanticToken {
                delta_line: 0,
                delta_start: 3,
                length: 1,
                token_type: 0,
                token_modifiers_bitset: 0,
            }],
        );

        assert_eq!(tokens[0].range, Range::new(2, 3));
    }

    #[test]
    fn semantic_overlay_uses_current_version_and_modifier_scope() {
        let theme: crate::Theme = toml::from_str(
            r##"
            "function" = "#00ff00"
            "semantic.function" = "#0000ff"
            "semantic.function.static" = "#ff0000"
            "##,
        )
        .unwrap();
        let mut tokens = HashMap::new();
        tokens.insert(
            LanguageServerId::default(),
            DocumentSemanticTokens {
                version: 7,
                tokens: vec![DocumentSemanticToken {
                    range: Range::new(2, 6),
                    token_type: "function".to_string(),
                    token_modifiers: vec!["static".to_string()],
                }],
            },
        );

        let overlay = semantic_tokens_overlay(&theme, &tokens, 7).expect("overlay");
        let OverlayHighlights::Heterogenous { highlights } = overlay else {
            panic!("semantic tokens use heterogeneous overlay highlights");
        };

        assert_eq!(highlights.len(), 1);
        assert_eq!(theme.scope(highlights[0].0), "semantic.function.static");
        assert_eq!(highlights[0].1, 2..6);
        assert!(semantic_tokens_overlay(&theme, &tokens, 8).is_none());
    }

    #[test]
    fn inline_completion_cancel_dismisses_ghost_text() {
        let mut state = DocumentLspState::default();
        state.set_inline_completion(InlineCompletionGhost {
            view_id: ViewId::default(),
            version: 3,
            cursor: 4,
            text: "completion".to_string(),
            annotation: InlineAnnotation::new(4, "completion"),
            replace_range: None,
        });

        assert!(state.inline_completion().is_some());
        assert!(!state.cancel_inline_completion());
        assert!(state.inline_completion().is_none());
    }
}
