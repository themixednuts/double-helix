//! Offline Helix adapter for editor-neutral code briefings.

use std::{cmp::Reverse, collections::BTreeSet, sync::Arc};

use code_atlas::{
    AnalysisCoverage, Briefing, Claim, ClaimId, Evidence, EvidenceKind, NodeKey, Provenance,
    Section, Subject, SubjectKind, Unresolved, UnresolvedReason,
};
use helix_core::{chars::char_is_word, tree_sitter::Node, Range, RopeSlice};
use helix_view::{Document, DocumentId, Editor};

use crate::runtime::{
    ui::command::{BriefingTarget, CodeAtlasCommand},
    UiCommand,
};

const FILE_CLAIM_LIMIT: usize = 6;
const CHILD_CLAIM_LIMIT: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SymbolClass {
    Function,
    Type,
    Module,
}

impl SymbolClass {
    const fn label(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Type => "type",
            Self::Module => "module",
        }
    }
}

#[derive(Clone, Debug)]
struct SymbolRecord {
    subject: Subject,
    class: SymbolClass,
    range: Range,
    line: usize,
    lines: usize,
    is_public: bool,
    mentions: usize,
}

pub fn prepare(editor: &Editor) -> anyhow::Result<UiCommand> {
    let source_view = editor.focused_view_id();
    let source_document = editor.focused_document_id();
    let document = editor
        .document(source_document)
        .ok_or_else(|| anyhow::anyhow!("focused document no longer exists"))?;
    let briefing = briefing_for_document(document, source_document);
    Ok(UiCommand::CodeAtlas(CodeAtlasCommand::Open {
        briefing,
        source_document,
        source_view,
    }))
}

pub(crate) fn briefing_for_document(
    document: &Document,
    document_id: DocumentId,
) -> Briefing<BriefingTarget> {
    file_briefing(document, document_id, file_subject(document))
}

pub(crate) fn briefing_for_subject(
    document: &Document,
    document_id: DocumentId,
    subject: Subject,
    target: BriefingTarget,
) -> Briefing<BriefingTarget> {
    match subject.kind {
        SubjectKind::Symbol => symbol_briefing(document, document_id, subject, target.range),
        SubjectKind::File => file_briefing(document, document_id, subject),
        _ => file_briefing(document, document_id, file_subject(document)),
    }
}

fn file_subject(document: &Document) -> Subject {
    let label = document
        .path()
        .and_then(|path| path.file_name())
        .map_or_else(
            || document.display_name().into_owned(),
            |name| name.to_string_lossy().into_owned(),
        );
    let identity = document
        .uri()
        .map_or_else(|| label.clone(), |uri| uri.to_string());
    Subject::new(
        NodeKey::new(format!("file:{identity}")),
        SubjectKind::File,
        label,
    )
}

fn file_briefing(
    document: &Document,
    document_id: DocumentId,
    subject: Subject,
) -> Briefing<BriefingTarget> {
    let syntax_available = document.syntax().is_some()
        && document.text().len_bytes() <= helix_core::syntax::MAX_FULL_DOCUMENT_SYNTAX_BYTES;
    let symbols = collect_symbols(document);
    let line_count = document.text().len_lines();
    let summary = if !syntax_available {
        format!("{line_count} lines · syntax structure unavailable")
    } else {
        format!("{line_count} lines · {} symbols", symbols.len())
    };
    let mut briefing = Briefing::new(
        subject,
        summary,
        AnalysisCoverage {
            syntax_resolved: symbols.len(),
            syntax_total: if syntax_available { symbols.len() } else { 1 },
            lsp_resolved: None,
            lsp_total: None,
        },
    );

    if symbols.is_empty() {
        let evidence = Evidence::new(
            BriefingTarget {
                document: document_id,
                range: Range::new(0, document.text().len_chars()),
            },
            ":1",
            EvidenceKind::Definition,
        );
        briefing.add_section(Section::new(
            "Source",
            vec![Claim::new(
                ClaimId::new("source"),
                format!("{line_count} lines of source"),
                evidence,
                Provenance::SYNTAX,
            )],
        ));
        briefing.add_unresolved(if syntax_available {
            Unresolved::new(
                "No supported symbol boundaries were found",
                UnresolvedReason::UnsupportedStructure,
            )
        } else {
            Unresolved::new(
                "Semantic structure is unavailable for this file",
                UnresolvedReason::ToolUnavailable,
            )
        });
        return briefing;
    }

    let types = symbols
        .iter()
        .filter(|symbol| symbol.class == SymbolClass::Type)
        .map(|symbol| symbol_claim(document, document_id, symbol))
        .collect();
    briefing.add_section(Section::new(
        "Types",
        bounded_claims(types, FILE_CLAIM_LIMIT, "types", "source order"),
    ));

    let starts = symbols
        .iter()
        .filter(|symbol| symbol.class == SymbolClass::Function && symbol.is_public)
        .map(|symbol| symbol_claim(document, document_id, symbol))
        .collect();
    briefing.add_section(Section::new(
        "Starts here",
        bounded_claims(starts, FILE_CLAIM_LIMIT, "entry-points", "public functions"),
    ));

    let mut work = symbols
        .iter()
        .filter(|symbol| symbol.class == SymbolClass::Function && !symbol.is_public)
        .collect::<Vec<_>>();
    work.sort_by_key(|symbol| Reverse(symbol.lines));
    let work = work
        .into_iter()
        .map(|symbol| symbol_claim(document, document_id, symbol))
        .collect();
    briefing.add_section(Section::new(
        "Does the work",
        bounded_claims(work, FILE_CLAIM_LIMIT, "work", "largest functions first"),
    ));

    briefing.add_section(Section::new(
        "Touches",
        touch_claims(
            document.text().slice(..),
            document_id,
            Range::new(0, document.text().len_chars()),
        ),
    ));

    add_file_unknowns(&mut briefing, document, document_id, &symbols);
    briefing
}

fn symbol_briefing(
    document: &Document,
    document_id: DocumentId,
    subject: Subject,
    range: Range,
) -> Briefing<BriefingTarget> {
    let text = document.text().slice(..);
    let node = semantic_node_for_range(document, range);
    let class = node
        .as_ref()
        .and_then(|node| symbol_class(node.kind()))
        .unwrap_or(SymbolClass::Function);
    let line_start = text.char_to_line(range.from()).saturating_add(1);
    let line_end = text
        .char_to_line(range.to().saturating_sub(1).min(text.len_chars()))
        .saturating_add(1);
    let source = text.to_string();
    let mentions = word_occurrences(&source, &subject.label).len();
    let mut briefing = Briefing::new(
        subject.clone(),
        format!(
            "{} · lines {line_start}–{line_end} · {mentions} file mentions",
            class.label()
        ),
        AnalysisCoverage {
            syntax_resolved: usize::from(node.is_some()),
            syntax_total: 1,
            lsp_resolved: None,
            lsp_total: None,
        },
    );

    let definition = Evidence::new(
        BriefingTarget {
            document: document_id,
            range,
        },
        format!(":{line_start}"),
        EvidenceKind::Definition,
    );
    briefing.add_section(Section::new(
        "Definition",
        vec![Claim::new(
            ClaimId::new(format!("definition:{}", subject.key)),
            format!("{} {}", class.label(), subject.label),
            definition,
            Provenance::SYNTAX,
        )],
    ));

    if let Some(node) = node {
        briefing.add_section(Section::new(
            "Calls in source order",
            call_claims(node, text, document_id),
        ));
    }
    briefing.add_section(Section::new(
        "Mentioned in this file",
        reference_claims(text, document_id, &subject, range),
    ));
    briefing.add_section(Section::new(
        "Touches",
        touch_claims(text, document_id, range),
    ));
    add_range_unknowns(&mut briefing, text, document_id, range);
    briefing
}

fn collect_symbols(document: &Document) -> Vec<SymbolRecord> {
    if document.text().len_bytes() > helix_core::syntax::MAX_FULL_DOCUMENT_SYNTAX_BYTES {
        return Vec::new();
    }
    let Some(syntax) = document.syntax() else {
        return Vec::new();
    };
    let text = document.text().slice(..);
    let source = text.to_string();
    let mut records = Vec::new();
    collect_symbol_nodes(syntax.tree().root_node(), text, &source, &mut records);
    records.sort_by_key(|record| record.range.from());
    records
}

fn collect_symbol_nodes(
    node: Node<'_>,
    text: RopeSlice<'_>,
    source: &str,
    records: &mut Vec<SymbolRecord>,
) {
    if let Some(class) = symbol_class(node.kind()) {
        if let Some(name) = node_name(&node, text) {
            let range = node_range(&node, text);
            let line = text.char_to_line(range.from()).saturating_add(1);
            let end_line = text
                .char_to_line(range.to().saturating_sub(1).min(text.len_chars()))
                .saturating_add(1);
            let key = NodeKey::new(format!(
                "syntax:{}:{}:{}",
                node.start_byte(),
                node.end_byte(),
                node.kind()
            ));
            let subject = Subject::new(key, SubjectKind::Symbol, name.clone());
            records.push(SymbolRecord {
                subject,
                class,
                range,
                line,
                lines: end_line.saturating_sub(line).saturating_add(1),
                is_public: is_public_node(&node, text),
                mentions: word_occurrences(source, &name).len(),
            });
        }
    }

    for child in node.children().filter(Node::is_named) {
        collect_symbol_nodes(child, text, source, records);
    }
}

fn symbol_claim(
    document: &Document,
    document_id: DocumentId,
    symbol: &SymbolRecord,
) -> Claim<BriefingTarget> {
    let evidence = Evidence::new(
        BriefingTarget {
            document: document_id,
            range: symbol.range,
        },
        format!(":{}", symbol.line),
        EvidenceKind::Definition,
    );
    let visibility = if symbol.is_public { "public · " } else { "" };
    let mention_label = match symbol.mentions {
        1 => "1 file mention".to_owned(),
        count => format!("{count} file mentions"),
    };
    let mut claim = Claim::new(
        ClaimId::new(symbol.subject.key.as_str()),
        Arc::clone(&symbol.subject.label),
        evidence,
        Provenance::SYNTAX,
    )
    .with_annotation(format!(
        "{visibility}{} · {mention_label}",
        symbol.class.label()
    ))
    .with_zoom(symbol.subject.clone());

    if symbol.class == SymbolClass::Function {
        if let Some(node) = semantic_node_for_range(document, symbol.range) {
            claim = claim.with_children(call_claims(node, document.text().slice(..), document_id));
        }
    }
    claim
}

fn call_claims(
    node: Node<'_>,
    text: RopeSlice<'_>,
    document_id: DocumentId,
) -> Vec<Claim<BriefingTarget>> {
    let mut calls = Vec::new();
    collect_calls(node, text, document_id, &mut calls);
    if calls.len() > CHILD_CLAIM_LIMIT {
        calls.truncate(CHILD_CLAIM_LIMIT);
    }
    calls
}

fn collect_calls(
    node: Node<'_>,
    text: RopeSlice<'_>,
    document_id: DocumentId,
    calls: &mut Vec<Claim<BriefingTarget>>,
) {
    if calls.len() >= CHILD_CLAIM_LIMIT {
        return;
    }
    if node.kind().to_ascii_lowercase().contains("call") {
        let called = node.named_child(0).map_or_else(
            || excerpt(&node, text, 42),
            |function| excerpt(&function, text, 42),
        );
        let range = node_range(&node, text);
        let line = text.char_to_line(range.from()).saturating_add(1);
        calls.push(
            Claim::new(
                ClaimId::new(format!("call:{}:{}", node.start_byte(), node.end_byte())),
                called,
                Evidence::new(
                    BriefingTarget {
                        document: document_id,
                        range,
                    },
                    format!(":{line}"),
                    EvidenceKind::Call,
                ),
                Provenance::SYNTAX,
            )
            .with_annotation("syntactic call"),
        );
    }
    for child in node.children().filter(Node::is_named) {
        collect_calls(child, text, document_id, calls);
    }
}

fn reference_claims(
    text: RopeSlice<'_>,
    document_id: DocumentId,
    subject: &Subject,
    definition: Range,
) -> Vec<Claim<BriefingTarget>> {
    let source = text.to_string();
    word_occurrences(&source, &subject.label)
        .into_iter()
        .filter_map(|byte| {
            let start = text.byte_to_char(byte);
            if start >= definition.from() && start < definition.to() {
                return None;
            }
            let end = start.saturating_add(subject.label.chars().count());
            let line = text.char_to_line(start).saturating_add(1);
            let line_range = text.line(line.saturating_sub(1));
            Some(
                Claim::new(
                    ClaimId::new(format!("reference:{byte}")),
                    compact(line_range.to_string(), 52),
                    Evidence::new(
                        BriefingTarget {
                            document: document_id,
                            range: Range::new(start, end),
                        },
                        format!(":{line}"),
                        EvidenceKind::Reference,
                    ),
                    Provenance::SYNTAX,
                )
                .with_annotation("textual mention"),
            )
        })
        .take(FILE_CLAIM_LIMIT)
        .collect()
}

fn touch_claims(
    text: RopeSlice<'_>,
    document_id: DocumentId,
    range: Range,
) -> Vec<Claim<BriefingTarget>> {
    const TOUCHES: &[(&str, &str)] = &[
        (".await", "awaits asynchronous work"),
        ("spawn(", "spawns background work"),
        ("unsafe", "uses unsafe code"),
        ("std::fs", "touches the filesystem"),
        ("fs::", "touches the filesystem"),
        ("Command::new", "starts an external process"),
        (".send(", "sends through a channel"),
    ];
    let start_byte = text.char_to_byte(range.from().min(text.len_chars()));
    let end_byte = text.char_to_byte(range.to().min(text.len_chars()));
    let source = text.to_string();
    let mut claims = Vec::new();
    let mut seen = BTreeSet::new();
    for (pattern, lead) in TOUCHES {
        for (relative, _) in source[start_byte..end_byte].match_indices(pattern).take(2) {
            let byte = start_byte + relative;
            let start = text.byte_to_char(byte);
            let end = text.byte_to_char(byte + pattern.len());
            let line = text.char_to_line(start).saturating_add(1);
            if !seen.insert((*lead, line)) {
                continue;
            }
            claims.push(
                Claim::new(
                    ClaimId::new(format!("touch:{pattern}:{byte}")),
                    *lead,
                    Evidence::new(
                        BriefingTarget {
                            document: document_id,
                            range: Range::new(start, end),
                        },
                        format!(":{line}"),
                        EvidenceKind::Boundary,
                    ),
                    Provenance::SYNTAX,
                )
                .with_annotation("text match"),
            );
        }
    }
    claims.sort_by_key(|claim| claim.evidence[0].target.range.from());
    claims.truncate(FILE_CLAIM_LIMIT);
    claims
}

fn add_file_unknowns(
    briefing: &mut Briefing<BriefingTarget>,
    document: &Document,
    document_id: DocumentId,
    symbols: &[SymbolRecord],
) {
    let text = document.text().slice(..);
    add_range_unknowns(briefing, text, document_id, Range::new(0, text.len_chars()));
    for symbol in symbols.iter().filter(|symbol| symbol.mentions <= 1).take(3) {
        briefing.add_unresolved(
            Unresolved::new(
                format!("{} has no other mention in this file", symbol.subject.label),
                UnresolvedReason::Unreferenced,
            )
            .with_candidate(Evidence::new(
                BriefingTarget {
                    document: document_id,
                    range: symbol.range,
                },
                format!(":{}", symbol.line),
                EvidenceKind::Candidate,
            )),
        );
    }
}

fn add_range_unknowns(
    briefing: &mut Briefing<BriefingTarget>,
    text: RopeSlice<'_>,
    document_id: DocumentId,
    range: Range,
) {
    let source = text.to_string();
    let start_byte = text.char_to_byte(range.from().min(text.len_chars()));
    let end_byte = text.char_to_byte(range.to().min(text.len_chars()));
    let slice = &source[start_byte..end_byte];
    if let Some(relative) = slice.find("dyn ") {
        let byte = start_byte + relative;
        let start = text.byte_to_char(byte);
        let line = text.char_to_line(start).saturating_add(1);
        briefing.add_unresolved(
            Unresolved::new(
                "Dynamic dispatch target cannot be selected statically",
                UnresolvedReason::DynamicDispatch,
            )
            .with_candidate(Evidence::new(
                BriefingTarget {
                    document: document_id,
                    range: Range::new(start, start.saturating_add(3)),
                },
                format!(":{line}"),
                EvidenceKind::Candidate,
            )),
        );
    }
    if let Some(relative) = slice.find("#[derive") {
        let byte = start_byte + relative;
        let start = text.byte_to_char(byte);
        let line = text.char_to_line(start).saturating_add(1);
        briefing.add_unresolved(
            Unresolved::new(
                "Derived behavior is not expanded in this briefing",
                UnresolvedReason::MacroExpansion,
            )
            .with_candidate(Evidence::new(
                BriefingTarget {
                    document: document_id,
                    range: Range::new(start, start.saturating_add(8)),
                },
                format!(":{line}"),
                EvidenceKind::Candidate,
            )),
        );
    }
}

fn bounded_claims(
    mut claims: Vec<Claim<BriefingTarget>>,
    limit: usize,
    id: &str,
    criterion: &str,
) -> Vec<Claim<BriefingTarget>> {
    if claims.len() <= limit {
        return claims;
    }
    let hidden = claims.split_off(limit);
    let evidence = hidden[0].evidence[0].clone();
    claims.push(
        Claim::new(
            ClaimId::new(format!("remainder:{id}")),
            format!("+ {} more", hidden.len()),
            evidence,
            Provenance::SYNTAX,
        )
        .with_annotation(criterion)
        .with_children(hidden),
    );
    claims
}

fn semantic_node_for_range(document: &Document, range: Range) -> Option<Node<'_>> {
    let text = document.text().slice(..);
    let start = text.char_to_byte(range.from().min(text.len_chars())) as u32;
    let end = text.char_to_byte(range.to().min(text.len_chars())) as u32;
    document
        .syntax()?
        .named_descendant_for_byte_range(start, end)
}

fn symbol_class(kind: &str) -> Option<SymbolClass> {
    let kind = kind.to_ascii_lowercase();
    if is_declaration_kind(&kind, &["function", "method", "constructor"]) {
        Some(SymbolClass::Function)
    } else if is_declaration_kind(
        &kind,
        &[
            "struct",
            "class",
            "interface",
            "trait",
            "enum",
            "union",
            "record",
            "type_alias",
        ],
    ) || kind == "type_item"
    {
        Some(SymbolClass::Type)
    } else if is_declaration_kind(&kind, &["mod", "module", "namespace"]) {
        Some(SymbolClass::Module)
    } else {
        None
    }
}

fn is_declaration_kind(kind: &str, stems: &[&str]) -> bool {
    const SUFFIXES: &[&str] = &[
        "_item",
        "_definition",
        "_declaration",
        "_signature",
        "_specifier",
    ];
    stems.contains(&kind)
        || SUFFIXES.iter().any(|suffix| {
            kind.strip_suffix(suffix)
                .is_some_and(|stem| stems.contains(&stem))
        })
}

fn node_name(node: &Node<'_>, text: RopeSlice<'_>) -> Option<String> {
    find_name_node(node)
        .map(|name| excerpt(&name, text, 48))
        .filter(|name| !name.is_empty())
}

fn find_name_node<'tree>(node: &Node<'tree>) -> Option<Node<'tree>> {
    let is_name = |child: &Node<'tree>| {
        let kind = child.kind().to_ascii_lowercase();
        child.is_named()
            && (kind.contains("identifier") || kind == "name" || kind.ends_with("_name"))
    };
    if let Some(name) = node.children().find(&is_name) {
        return Some(name);
    }
    node.children()
        .filter(Node::is_named)
        .find_map(|child| child.children().find(&is_name))
}

fn is_public_node(node: &Node<'_>, text: RopeSlice<'_>) -> bool {
    let excerpt = excerpt(node, text, 80);
    let prefix = excerpt.trim_start();
    prefix.starts_with("pub ")
        || prefix.starts_with("pub(")
        || prefix.starts_with("export ")
        || prefix.starts_with("public ")
}

fn node_range(node: &Node<'_>, text: RopeSlice<'_>) -> Range {
    Range::new(
        text.byte_to_char(node.start_byte() as usize),
        text.byte_to_char(node.end_byte() as usize),
    )
}

fn excerpt(node: &Node<'_>, text: RopeSlice<'_>, limit: usize) -> String {
    let range = node_range(node, text);
    compact(text.slice(range.from()..range.to()).to_string(), limit)
}

fn compact(text: String, limit: usize) -> String {
    let mut compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() > limit {
        compact = compact.chars().take(limit.saturating_sub(1)).collect();
        compact.push('…');
    }
    compact
}

fn word_occurrences(source: &str, word: &str) -> Vec<usize> {
    if word.is_empty() {
        return Vec::new();
    }
    source
        .match_indices(word)
        .filter_map(|(start, _)| {
            let before = source[..start].chars().next_back();
            let end = start + word.len();
            let after = source[end..].chars().next();
            (!before.is_some_and(char_is_word) && !after.is_some_and(char_is_word)).then_some(start)
        })
        .collect()
}

#[cfg(test)]
mod code_atlas_tests {
    use super::*;
    use arc_swap::ArcSwap;
    use helix_core::Rope;
    use helix_view::editor::Config;
    use std::sync::Arc;

    fn rust_document(source: &str) -> Document {
        let loader = Arc::new(ArcSwap::from_pointee(
            helix_core::config::default_lang_loader(),
        ));
        let config = Arc::new(ArcSwap::new(Arc::new(Config::default())));
        let mut document = Document::from(Rope::from(source), None, config, loader.clone());
        document
            .set_language_by_language_id("rust", &loader.load())
            .unwrap();
        let syntax = document
            .prepare_syntax_refresh()
            .expect("Rust syntax refresh")
            .execute()
            .expect("Rust syntax tree");
        document.set_syntax(Some(syntax));
        document
    }

    #[test]
    fn compact_collapses_whitespace_and_bounds_output() {
        assert_eq!(compact("  fn   hello ()  ".into(), 20), "fn hello ()");
        assert_eq!(compact("abcdefghijkl".into(), 8), "abcdefg…");
    }

    #[test]
    fn syntax_kinds_map_only_semantic_boundaries() {
        assert_eq!(symbol_class("function_item"), Some(SymbolClass::Function));
        assert_eq!(symbol_class("struct_item"), Some(SymbolClass::Type));
        assert_eq!(symbol_class("mod_item"), Some(SymbolClass::Module));
        assert_eq!(symbol_class("call_expression"), None);
        assert_eq!(symbol_class("function_type"), None);
        assert_eq!(symbol_class("class_body"), None);
        assert_eq!(symbol_class("struct_pattern"), None);
        assert_eq!(symbol_class("identifier"), None);
    }

    #[test]
    fn textual_mentions_respect_word_boundaries() {
        assert_eq!(word_occurrences("pick picker pick", "pick"), vec![0, 12]);
    }

    #[test]
    fn rust_file_briefing_groups_definitions_and_boundaries() {
        let document = rust_document(
            r#"
                pub struct Worker;
                pub fn run() { helper(); }
                fn helper() { let _ = std::fs::read("x"); }
            "#,
        );
        let document_id = DocumentId::default();
        let briefing = file_briefing(&document, document_id, file_subject(&document));

        let section = |title| {
            briefing
                .sections
                .iter()
                .find(|section| section.title.as_ref() == title)
                .expect("briefing section")
        };
        assert!(section("Types")
            .claims
            .iter()
            .any(|claim| claim.lead.as_ref() == "Worker"));
        assert!(section("Starts here")
            .claims
            .iter()
            .any(|claim| claim.lead.as_ref() == "run"));
        assert!(section("Does the work")
            .claims
            .iter()
            .any(|claim| claim.lead.as_ref() == "helper"));
        assert!(section("Touches")
            .claims
            .iter()
            .any(|claim| claim.lead.as_ref() == "touches the filesystem"));
        assert!(briefing
            .sections
            .iter()
            .flat_map(|section| &section.claims)
            .all(|claim| !claim.evidence.is_empty()));
    }
}
