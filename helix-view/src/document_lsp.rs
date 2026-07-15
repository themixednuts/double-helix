use std::collections::{HashMap, HashSet};
use std::ops::Range as StdRange;
use std::sync::Arc;

use crate::ViewId;
use helix_core::syntax::{self, OverlayHighlights};
use helix_core::text_annotations::InlineAnnotation;
use helix_core::{Assoc, ChangeSet, Range, Rope};
use helix_lsp::{lsp, LanguageServerId, OffsetEncoding};
use helix_runtime::Token;

#[derive(Debug, Clone, Default)]
pub struct DocumentColorSwatches {
    pub color_swatches: Arc<[InlineAnnotation]>,
    pub colors: Arc<[syntax::Highlight]>,
    pub color_swatches_padding: Arc<[InlineAnnotation]>,
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

#[derive(Debug, Clone, Default)]
pub struct DocumentSemanticTokenDeltaState {
    pub result_id: Option<String>,
    pub data: Vec<lsp::SemanticToken>,
}

#[derive(Debug, Clone)]
pub struct DocumentSemanticTokenUpdate {
    pub version: i32,
    pub result_id: Option<String>,
    pub data: Vec<lsp::SemanticToken>,
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
    pub annotations: Arc<[InlineAnnotation]>,
}

#[derive(Debug, Default)]
pub struct DocumentLspState {
    previous_diagnostic_ids: HashMap<LanguageServerId, String>,
    pull_diagnostic_generations: HashMap<LanguageServerId, u64>,
    color_swatches: Option<Arc<DocumentColorSwatches>>,
    code_lenses: Option<DocumentCodeLenses>,
    document_links: Option<DocumentLinks>,
    semantic_tokens: HashMap<LanguageServerId, DocumentSemanticTokens>,
    semantic_token_delta_states: HashMap<LanguageServerId, DocumentSemanticTokenDeltaState>,
    inline_completion: Option<Arc<InlineCompletionGhost>>,
    inline_values: Option<Arc<DocumentInlineValues>>,
    color_swatch_cancel: Option<Token>,
    code_lens_cancel: Option<Token>,
    document_link_cancel: Option<Token>,
    folding_range_cancel: Option<Token>,
    semantic_token_cancel: Option<Token>,
    inline_completion_cancel: Option<Token>,
    inline_value_cancel: Option<Token>,
    lsp_fold_views: HashSet<ViewId>,
}

impl DocumentLspState {
    fn is_current_request(current: &Option<Token>, request: &Token) -> bool {
        !request.is_canceled()
            && current
                .as_ref()
                .is_some_and(|current| current.same_token(request))
    }

    pub fn next_pull_diagnostics_generation(&mut self, server_id: LanguageServerId) -> u64 {
        let generation = self
            .pull_diagnostic_generations
            .entry(server_id)
            .or_default();
        *generation = generation.saturating_add(1).max(1);
        *generation
    }

    pub fn pull_diagnostics_generation(&self, server_id: LanguageServerId) -> Option<u64> {
        self.pull_diagnostic_generations.get(&server_id).copied()
    }

    pub fn is_current_pull_diagnostics(
        &self,
        server_id: LanguageServerId,
        generation: u64,
    ) -> bool {
        self.pull_diagnostics_generation(server_id) == Some(generation)
    }

    pub fn previous_diagnostic_id(&self, server_id: LanguageServerId) -> Option<&str> {
        self.previous_diagnostic_ids
            .get(&server_id)
            .map(String::as_str)
    }

    pub fn set_previous_diagnostic_id(
        &mut self,
        server_id: LanguageServerId,
        previous_diagnostic_id: Option<String>,
    ) {
        match previous_diagnostic_id {
            Some(previous_diagnostic_id) => {
                self.previous_diagnostic_ids
                    .insert(server_id, previous_diagnostic_id);
            }
            None => {
                self.previous_diagnostic_ids.remove(&server_id);
            }
        }
    }

    pub fn clear_pull_diagnostics_server(&mut self, server_id: LanguageServerId) {
        self.previous_diagnostic_ids.remove(&server_id);
        self.pull_diagnostic_generations.remove(&server_id);
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

    pub fn is_current_color_swatches(&self, request: &Token) -> bool {
        Self::is_current_request(&self.color_swatch_cancel, request)
    }

    pub fn color_swatches(&self) -> Option<&DocumentColorSwatches> {
        self.color_swatches.as_deref()
    }

    pub fn color_swatches_snapshot(&self) -> Option<Arc<DocumentColorSwatches>> {
        self.color_swatches.clone()
    }

    pub fn clear_color_swatches(&mut self) {
        self.color_swatches = None;
    }

    pub fn set_color_swatches(&mut self, color_swatches: DocumentColorSwatches) {
        self.color_swatches = Some(Arc::new(color_swatches));
    }

    pub fn update_color_swatches(&mut self, changes: &ChangeSet) {
        let apply_color_swatch_changes = |annotations: &mut Arc<[InlineAnnotation]>| {
            changes.update_positions(
                Arc::make_mut(annotations)
                    .iter_mut()
                    .map(|annotation| (&mut annotation.char_idx, Assoc::After)),
            );
        };

        if let Some(DocumentColorSwatches {
            color_swatches,
            colors: _,
            color_swatches_padding,
        }) = self.color_swatches.as_mut().map(Arc::make_mut)
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

    pub fn is_current_code_lenses(&self, request: &Token) -> bool {
        Self::is_current_request(&self.code_lens_cancel, request)
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

    pub fn is_current_document_links(&self, request: &Token) -> bool {
        Self::is_current_request(&self.document_link_cancel, request)
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

    pub fn is_current_semantic_tokens(&self, request: &Token) -> bool {
        Self::is_current_request(&self.semantic_token_cancel, request)
    }

    pub fn semantic_tokens(&self) -> &HashMap<LanguageServerId, DocumentSemanticTokens> {
        &self.semantic_tokens
    }

    pub fn semantic_token_delta_state(
        &self,
        server_id: LanguageServerId,
    ) -> Option<&DocumentSemanticTokenDeltaState> {
        self.semantic_token_delta_states.get(&server_id)
    }

    pub fn clear_semantic_tokens(&mut self) {
        self.semantic_tokens.clear();
        self.semantic_token_delta_states.clear();
    }

    pub fn set_semantic_tokens(
        &mut self,
        server_id: LanguageServerId,
        tokens: DocumentSemanticTokens,
    ) {
        self.semantic_tokens.insert(server_id, tokens);
    }

    pub fn set_semantic_token_update(
        &mut self,
        server_id: LanguageServerId,
        update: DocumentSemanticTokenUpdate,
    ) {
        self.semantic_token_delta_states.insert(
            server_id,
            DocumentSemanticTokenDeltaState {
                result_id: update.result_id,
                data: update.data,
            },
        );
        self.semantic_tokens.insert(
            server_id,
            DocumentSemanticTokens {
                version: update.version,
                tokens: update.tokens,
            },
        );
    }

    pub fn update_semantic_tokens(&mut self, changes: &ChangeSet) {
        self.semantic_tokens.clear();
        self.semantic_token_delta_states.clear();
        if let Some(ghost) = self.inline_completion.as_mut().map(Arc::make_mut) {
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
        if let Some(inline_values) = self.inline_values.as_mut().map(Arc::make_mut) {
            changes.update_positions(
                Arc::make_mut(&mut inline_values.annotations)
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

    pub fn is_current_inline_completion(&self, request: &Token) -> bool {
        Self::is_current_request(&self.inline_completion_cancel, request)
    }

    pub fn inline_completion(&self) -> Option<&InlineCompletionGhost> {
        self.inline_completion.as_deref()
    }

    pub fn inline_completion_snapshot(&self) -> Option<Arc<InlineCompletionGhost>> {
        self.inline_completion.clone()
    }

    pub fn set_inline_completion(&mut self, completion: InlineCompletionGhost) {
        self.inline_completion = Some(Arc::new(completion));
    }

    pub fn clear_inline_completion(&mut self) {
        self.inline_completion = None;
    }

    pub fn restart_inline_values(&mut self) -> Token {
        self.cancel_inline_values();
        self.inline_values = None;
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
        self.inline_values = None;
        was_active
    }

    pub fn is_current_inline_values(&self, request: &Token) -> bool {
        Self::is_current_request(&self.inline_value_cancel, request)
    }

    pub fn inline_values(&self) -> Option<&DocumentInlineValues> {
        self.inline_values.as_deref()
    }

    pub fn inline_values_snapshot(&self) -> Option<Arc<DocumentInlineValues>> {
        self.inline_values.clone()
    }

    pub fn set_inline_values(&mut self, values: DocumentInlineValues) {
        self.inline_values = Some(Arc::new(values));
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

    pub fn is_current_folding_ranges(&self, request: &Token) -> bool {
        Self::is_current_request(&self.folding_range_cancel, request)
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

fn pack_semantic_tokens(tokens: &[lsp::SemanticToken]) -> Vec<u32> {
    let mut data = Vec::with_capacity(tokens.len() * 5);
    for token in tokens {
        data.extend([
            token.delta_line,
            token.delta_start,
            token.length,
            token.token_type,
            token.token_modifiers_bitset,
        ]);
    }
    data
}

fn unpack_semantic_tokens(data: Vec<u32>) -> Option<Vec<lsp::SemanticToken>> {
    let chunks = data.chunks_exact(5);
    chunks.remainder().is_empty().then(|| {
        chunks
            .map(|chunk| lsp::SemanticToken {
                delta_line: chunk[0],
                delta_start: chunk[1],
                length: chunk[2],
                token_type: chunk[3],
                token_modifiers_bitset: chunk[4],
            })
            .collect()
    })
}

pub fn apply_semantic_token_delta_edits(
    tokens: &[lsp::SemanticToken],
    edits: &[lsp::SemanticTokensEdit],
) -> Option<Vec<lsp::SemanticToken>> {
    let mut data = pack_semantic_tokens(tokens);
    let mut edits = edits.iter().collect::<Vec<_>>();
    edits.sort_by_key(|edit| edit.start);

    for edit in edits.into_iter().rev() {
        let start = edit.start as usize;
        let delete_count = edit.delete_count as usize;
        let end = start.checked_add(delete_count)?;
        if end > data.len() {
            return None;
        }
        let replacement = edit.data.clone().unwrap_or_default();
        data.splice(start..end, replacement);
    }

    unpack_semantic_tokens(data)
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
    viewport: Option<StdRange<usize>>,
) -> Option<OverlayHighlights> {
    let mut highlights = tokens
        .values()
        .filter(|set| set.version == version)
        .flat_map(|set| {
            set.tokens.iter().filter_map(|token| {
                if viewport.as_ref().is_some_and(|viewport| {
                    token.range.to() < viewport.start || token.range.from() > viewport.end
                }) {
                    return None;
                }
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

        let overlay = semantic_tokens_overlay(&theme, &tokens, 7, None).expect("overlay");
        let OverlayHighlights::Heterogenous { highlights } = overlay else {
            panic!("semantic tokens use heterogeneous overlay highlights");
        };

        assert_eq!(highlights.len(), 1);
        assert_eq!(theme.scope(highlights[0].0), "semantic.function.static");
        assert_eq!(highlights[0].1, 2..6);
        assert!(semantic_tokens_overlay(&theme, &tokens, 8, None).is_none());
        assert!(semantic_tokens_overlay(&theme, &tokens, 7, Some(7..9)).is_none());
        assert!(semantic_tokens_overlay(&theme, &tokens, 7, Some(0..2)).is_some());
    }

    #[test]
    fn semantic_token_delta_edits_apply_to_packed_stream() {
        let before = vec![
            lsp::SemanticToken {
                delta_line: 2,
                delta_start: 5,
                length: 3,
                token_type: 0,
                token_modifiers_bitset: 3,
            },
            lsp::SemanticToken {
                delta_line: 0,
                delta_start: 5,
                length: 4,
                token_type: 1,
                token_modifiers_bitset: 0,
            },
            lsp::SemanticToken {
                delta_line: 3,
                delta_start: 2,
                length: 7,
                token_type: 2,
                token_modifiers_bitset: 0,
            },
        ];
        let edits = vec![lsp::SemanticTokensEdit {
            start: 0,
            delete_count: 1,
            data: Some(vec![3]),
        }];

        let after = apply_semantic_token_delta_edits(&before, &edits).unwrap();

        assert_eq!(
            pack_semantic_tokens(&after),
            vec![3, 5, 3, 0, 3, 0, 5, 4, 1, 0, 3, 2, 7, 2, 0]
        );
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

    #[test]
    fn inline_value_restart_clears_and_cancels_previous_frame_values() {
        let mut state = DocumentLspState::default();
        let first = state.restart_inline_values();
        state.set_inline_values(DocumentInlineValues {
            annotations: vec![InlineAnnotation::new(3, " = 1")].into(),
        });

        let second = state.restart_inline_values();

        assert!(first.is_canceled());
        assert!(!second.is_canceled());
        assert!(state.inline_values().is_none());
    }

    #[test]
    fn feature_results_only_match_the_current_request_ticket() {
        let mut state = DocumentLspState::default();
        let first = state.restart_code_lenses();
        let second = state.restart_code_lenses();

        assert!(first.is_canceled());
        assert!(!state.is_current_code_lenses(&first));
        assert!(state.is_current_code_lenses(&second));

        state.cancel_code_lenses();
        assert!(!state.is_current_code_lenses(&second));
    }

    #[test]
    fn pull_diagnostic_generations_and_result_ids_are_server_scoped() {
        let mut server_ids = slotmap::SlotMap::<LanguageServerId, ()>::with_key();
        let first_server = server_ids.insert(());
        let second_server = server_ids.insert(());
        let mut state = DocumentLspState::default();

        let first_generation = state.next_pull_diagnostics_generation(first_server);
        let second_generation = state.next_pull_diagnostics_generation(second_server);
        state.set_previous_diagnostic_id(first_server, Some("first-result".to_string()));
        state.set_previous_diagnostic_id(second_server, Some("second-result".to_string()));

        assert_eq!(first_generation, 1);
        assert_eq!(second_generation, 1);
        assert!(state.is_current_pull_diagnostics(first_server, first_generation));
        assert!(state.is_current_pull_diagnostics(second_server, second_generation));
        assert_eq!(
            state.previous_diagnostic_id(first_server),
            Some("first-result")
        );
        assert_eq!(
            state.previous_diagnostic_id(second_server),
            Some("second-result")
        );

        state.next_pull_diagnostics_generation(first_server);
        state.set_previous_diagnostic_id(first_server, Some("first-result-2".to_string()));
        assert!(!state.is_current_pull_diagnostics(first_server, first_generation));
        assert_eq!(
            state.previous_diagnostic_id(second_server),
            Some("second-result")
        );

        state.clear_pull_diagnostics_server(first_server);
        assert_eq!(state.pull_diagnostics_generation(first_server), None);
        assert_eq!(state.previous_diagnostic_id(first_server), None);
        assert_eq!(
            state.previous_diagnostic_id(second_server),
            Some("second-result")
        );
    }
}
