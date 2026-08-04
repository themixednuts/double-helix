use std::collections::BTreeSet;

use code_atlas::{Briefing, ClaimId, OutlineLineKind, Subject};
use helix_core::{unicode::width::UnicodeWidthStr, Selection, Tendril, Transaction};
use helix_view::{
    document::Mode,
    editor::ClosePolicy,
    graphics::Rect,
    input::{Event, KeyCode, KeyModifiers},
    view::ViewPosition,
    DocumentId, Editor, ViewId,
};

use crate::{
    compositor::{Component, Context, EventResult, PostAction, RenderContext},
    runtime::ui::command::BriefingTarget,
};

pub const ID: &str = "code-atlas";

const CITATION_COLUMN: usize = 38;

#[derive(Clone, Debug, Default)]
struct BriefingLine {
    claim: Option<ClaimId>,
    target: Option<BriefingTarget>,
    zoom: Option<Subject>,
    foldable: bool,
    expanded: bool,
}

impl BriefingLine {
    fn is_actionable(&self) -> bool {
        self.target.is_some() || self.foldable || self.zoom.is_some()
    }
}

#[derive(Clone, Debug, Default)]
struct RenderedBriefing {
    text: String,
    lines: Vec<BriefingLine>,
    actionable: Vec<usize>,
}

impl RenderedBriefing {
    fn push(&mut self, text: impl AsRef<str>, line: BriefingLine) {
        let row = self.lines.len();
        self.text.push_str(text.as_ref());
        self.text.push('\n');
        if line.is_actionable() {
            self.actionable.push(row);
        }
        self.lines.push(line);
    }

    fn push_blank(&mut self) {
        self.push("", BriefingLine::default());
    }

    fn first_actionable(&self) -> Option<usize> {
        self.actionable.first().copied()
    }

    fn move_from(&self, current: usize, amount: isize) -> Option<usize> {
        if self.actionable.is_empty() {
            return None;
        }
        let insertion = self.actionable.partition_point(|line| *line < current);
        let destination = match self.actionable.binary_search(&current) {
            Ok(index) => index.saturating_add_signed(amount),
            Err(_) if amount > 0 => insertion.saturating_add(amount as usize - 1),
            Err(_) => insertion.saturating_sub(amount.unsigned_abs()),
        }
        .min(self.actionable.len() - 1);
        self.actionable.get(destination).copied()
    }
}

#[derive(Clone, Debug)]
struct ZoomFrame {
    briefing: Briefing<BriefingTarget>,
    expanded: BTreeSet<ClaimId>,
    selected_claim: Option<ClaimId>,
}

/// Input controller for a briefing scratch buffer and its synchronized source view.
///
/// Rendering remains the normal editor's job. This component only turns the generated
/// buffer into a navigable, read-only semantic index.
pub struct CodeAtlasSession {
    source_document: DocumentId,
    source_view: ViewId,
    source_selection: Selection,
    source_offset: ViewPosition,
    source_version: i32,
    briefing_document: DocumentId,
    briefing_view: ViewId,
    briefing: Briefing<BriefingTarget>,
    rendered: RenderedBriefing,
    expanded: BTreeSet<ClaimId>,
    inspected: BTreeSet<ClaimId>,
    zoom_stack: Vec<ZoomFrame>,
    last_preview_line: Option<usize>,
}

impl CodeAtlasSession {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        source_document: DocumentId,
        source_view: ViewId,
        source_selection: Selection,
        source_offset: ViewPosition,
        source_version: i32,
        briefing_document: DocumentId,
        briefing_view: ViewId,
        briefing: Briefing<BriefingTarget>,
    ) -> Self {
        let expanded = BTreeSet::new();
        let inspected = BTreeSet::new();
        let rendered = render_briefing(&briefing, &expanded, &inspected);
        Self {
            source_document,
            source_view,
            source_selection,
            source_offset,
            source_version,
            briefing_document,
            briefing_view,
            briefing,
            rendered,
            expanded,
            inspected,
            zoom_stack: Vec::new(),
            last_preview_line: None,
        }
    }

    pub fn initial_text(briefing: &Briefing<BriefingTarget>) -> String {
        render_briefing(briefing, &BTreeSet::new(), &BTreeSet::new()).text
    }

    pub fn initialize(&mut self, editor: &mut Editor) {
        if let Some(line) = self.rendered.first_actionable() {
            self.select_line(editor, line);
        }
    }

    pub fn dispose(&mut self, editor: &mut Editor, restore_source: bool) {
        let source_is_live = editor
            .tree
            .try_get(self.source_view)
            .is_some_and(|view| view.doc == self.source_document);
        if source_is_live && restore_source {
            if let Some(document) = editor.document_mut(self.source_document) {
                document.set_selection(self.source_view, self.source_selection.clone());
                document.set_view_offset(self.source_view, self.source_offset);
            }
        }
        let _ = editor.close_document(self.briefing_document, ClosePolicy::DiscardModified);
        if source_is_live {
            editor.focus(self.source_view);
        }
    }

    fn briefing_is_focused(&self, editor: &Editor) -> bool {
        editor.focused_document_id() == self.briefing_document
    }

    fn refresh_after_source_edit(&mut self, editor: &mut Editor) {
        let next = {
            let Some(document) = editor.document(self.source_document) else {
                return;
            };
            if document.version() == self.source_version
                || document.prepare_syntax_refresh().is_some()
            {
                return;
            }
            self.source_version = document.version();
            crate::commands::understanding::briefing_for_document(document, self.source_document)
        };
        self.briefing = next;
        self.expanded.clear();
        self.zoom_stack.clear();
        self.replace_text(editor, None);
    }

    fn selected_line(&self, editor: &Editor) -> Option<usize> {
        let document = editor.document(self.briefing_document)?;
        let cursor = document
            .selection(self.briefing_view)
            .primary()
            .cursor(document.text().slice(..));
        Some(
            document
                .text()
                .char_to_line(cursor.min(document.text().len_chars())),
        )
    }

    fn select_line(&mut self, editor: &mut Editor, line: usize) {
        let Some(document) = editor.document_mut(self.briefing_document) else {
            return;
        };
        let line = line.min(document.text().len_lines().saturating_sub(1));
        let position = document.text().line_to_char(line);
        document.set_selection(self.briefing_view, Selection::point(position));
        editor.ensure_cursor_in_view(self.briefing_view);
        self.preview_selected(editor);
    }

    fn preview_selected(&mut self, editor: &mut Editor) {
        let Some(line_index) = self.selected_line(editor) else {
            return;
        };
        if self.last_preview_line == Some(line_index) {
            return;
        }
        self.last_preview_line = Some(line_index);
        let Some(line) = self.rendered.lines.get(line_index) else {
            return;
        };
        if editor
            .document(self.source_document)
            .is_none_or(|document| document.version() != self.source_version)
        {
            return;
        }
        if let Some(claim) = &line.claim {
            self.inspected.insert(claim.clone());
        }
        let Some(target) = line.target else {
            return;
        };
        if target.document != self.source_document
            || editor
                .tree
                .try_get(self.source_view)
                .is_none_or(|view| view.doc != target.document)
        {
            return;
        }
        if let Some(document) = editor.document_mut(target.document) {
            let position = target.range.from().min(document.text().len_chars());
            document.set_selection(self.source_view, Selection::point(position));
        }
        editor.ensure_cursor_in_view(self.source_view);
    }

    fn move_selection(&mut self, editor: &mut Editor, amount: isize) {
        let current = self.selected_line(editor).unwrap_or(0);
        if let Some(line) = self.rendered.move_from(current, amount) {
            self.select_line(editor, line);
        }
    }

    fn select_edge(&mut self, editor: &mut Editor, last: bool) {
        let line = if last {
            self.rendered.actionable.last().copied()
        } else {
            self.rendered.first_actionable()
        };
        if let Some(line) = line {
            self.select_line(editor, line);
        }
    }

    fn toggle_fold(&mut self, editor: &mut Editor) {
        let Some(line) = self
            .selected_line(editor)
            .and_then(|line| self.rendered.lines.get(line))
            .cloned()
        else {
            return;
        };
        let Some(claim) = line.claim.filter(|_| line.foldable) else {
            return;
        };
        if line.expanded {
            self.expanded.remove(&claim);
        } else {
            self.expanded.insert(claim.clone());
        }
        self.replace_text(editor, Some(&claim));
    }

    fn zoom_in(&mut self, editor: &mut Editor) {
        let Some(line) = self
            .selected_line(editor)
            .and_then(|line| self.rendered.lines.get(line))
            .cloned()
        else {
            return;
        };
        let (Some(subject), Some(target)) = (line.zoom, line.target) else {
            return;
        };
        let next = {
            let Some(document) = editor.document(self.source_document) else {
                return;
            };
            crate::commands::understanding::briefing_for_subject(
                document,
                self.source_document,
                subject,
                target,
            )
        };
        self.zoom_stack.push(ZoomFrame {
            briefing: self.briefing.clone(),
            expanded: self.expanded.clone(),
            selected_claim: line.claim,
        });
        self.briefing = next;
        self.expanded.clear();
        self.replace_text(editor, None);
    }

    fn zoom_out(&mut self, editor: &mut Editor) {
        let Some(frame) = self.zoom_stack.pop() else {
            return;
        };
        self.briefing = frame.briefing;
        self.expanded = frame.expanded;
        self.replace_text(editor, frame.selected_claim.as_ref());
    }

    fn replace_text(&mut self, editor: &mut Editor, preferred_claim: Option<&ClaimId>) {
        self.rendered = render_briefing(&self.briefing, &self.expanded, &self.inspected);
        let preferred_line = preferred_claim.and_then(|claim| {
            self.rendered
                .lines
                .iter()
                .position(|line| line.claim.as_ref() == Some(claim))
        });
        let selected = preferred_line.or_else(|| self.rendered.first_actionable());
        let Some(document) = editor.document_mut(self.briefing_document) else {
            return;
        };
        let transaction = Transaction::change(
            document.text(),
            std::iter::once((
                0,
                document.text().len_chars(),
                Some(Tendril::from(self.rendered.text.clone())),
            )),
        );
        document.apply(&transaction, self.briefing_view);
        document.reset_modified();
        self.last_preview_line = None;
        if let Some(line) = selected {
            self.select_line(editor, line);
        }
    }

    fn focus_source(&self, editor: &mut Editor) {
        if editor.tree.contains(self.source_view) {
            editor.focus(self.source_view);
        }
    }
}

impl Component for CodeAtlasSession {
    fn sync(&mut self, _viewport: Rect, editor: &mut Editor) {
        if !self.briefing_is_focused(editor) {
            return;
        }
        if editor.mode != Mode::Normal {
            editor.enter_normal_mode();
        }
        self.refresh_after_source_edit(editor);
        self.preview_selected(editor);
    }

    fn handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        if !self.briefing_is_focused(cx.editor) {
            return EventResult::Ignored(None);
        }
        let Event::Key(key) = event else {
            return if matches!(event, Event::Paste(_)) {
                EventResult::Consumed(None)
            } else {
                EventResult::Ignored(None)
            };
        };

        match (key.code, key.modifiers) {
            (KeyCode::Char('q'), KeyModifiers::NONE) => {
                self.dispose(cx.editor, true);
                return EventResult::Consumed(Some(PostAction::RemoveById(ID)));
            }
            (KeyCode::Char('o') | KeyCode::Esc, KeyModifiers::NONE) => self.focus_source(cx.editor),
            (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE) => {
                self.move_selection(cx.editor, 1)
            }
            (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE) => {
                self.move_selection(cx.editor, -1)
            }
            (KeyCode::PageDown, KeyModifiers::NONE)
            | (KeyCode::Char('d'), KeyModifiers::CONTROL) => self.move_selection(cx.editor, 5),
            (KeyCode::PageUp, KeyModifiers::NONE) | (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                self.move_selection(cx.editor, -5)
            }
            (KeyCode::Home | KeyCode::Char('g'), KeyModifiers::NONE) => {
                self.select_edge(cx.editor, false)
            }
            (KeyCode::End | KeyCode::Char('G'), KeyModifiers::NONE) => {
                self.select_edge(cx.editor, true)
            }
            (KeyCode::Tab, KeyModifiers::NONE) => self.toggle_fold(cx.editor),
            (KeyCode::Enter, KeyModifiers::NONE) => self.zoom_in(cx.editor),
            (KeyCode::Char('-'), KeyModifiers::NONE) => self.zoom_out(cx.editor),
            _ => {}
        }
        EventResult::Consumed(None)
    }

    fn prepare_render(
        &mut self,
        area: Rect,
        _ctx: &RenderContext,
    ) -> crate::render::PreparedRender {
        crate::render::PreparedRender::ready(crate::render::RenderOutput::sparse(area))
    }

    fn id(&self) -> Option<&str> {
        Some(ID)
    }
}

fn render_briefing(
    briefing: &Briefing<BriefingTarget>,
    expanded: &BTreeSet<ClaimId>,
    inspected: &BTreeSet<ClaimId>,
) -> RenderedBriefing {
    let mut rendered = RenderedBriefing::default();
    let outline = briefing.outline(expanded);
    let subject_kind = outline
        .first()
        .and_then(|line| line.tail.as_deref())
        .unwrap_or("Subject")
        .to_owned();

    for line in outline {
        match line.kind {
            OutlineLineKind::Subject => rendered.push(
                line.lead.as_ref(),
                BriefingLine {
                    zoom: line.zoom,
                    ..BriefingLine::default()
                },
            ),
            OutlineLineKind::Summary => rendered.push(
                format!("{subject_kind} · {}", line.lead),
                BriefingLine::default(),
            ),
            OutlineLineKind::Section => {
                rendered.push_blank();
                rendered.push(line.lead.as_ref(), BriefingLine::default());
            }
            OutlineLineKind::Claim => {
                let was_inspected = line
                    .claim
                    .as_ref()
                    .is_some_and(|claim| inspected.contains(claim));
                let marker = if line.foldable {
                    if line.expanded {
                        "▾"
                    } else {
                        "▸"
                    }
                } else if was_inspected {
                    "·"
                } else {
                    " "
                };
                let indent = "  ".repeat(usize::from(line.depth));
                let lead = format!("{indent}{marker} {}", line.lead);
                let text = align_tail(&lead, line.tail.as_deref());
                rendered.push(
                    text,
                    BriefingLine {
                        claim: line.claim,
                        target: line.target,
                        zoom: line.zoom,
                        foldable: line.foldable,
                        expanded: line.expanded,
                    },
                );
            }
            OutlineLineKind::Unknown => {
                let indent = "  ".repeat(usize::from(line.depth));
                let lead = format!("{indent}? {}", line.lead);
                rendered.push(
                    align_tail(&lead, line.tail.as_deref()),
                    BriefingLine {
                        target: line.target,
                        ..BriefingLine::default()
                    },
                );
            }
            OutlineLineKind::Status => {
                rendered.push_blank();
                rendered.push(format!("Analysis · {}", line.lead), BriefingLine::default());
                rendered.push(
                    "j/k move · tab unfold · enter zoom · o source · - back · q close",
                    BriefingLine::default(),
                );
            }
        }
    }
    rendered
}

fn align_tail(lead: &str, tail: Option<&str>) -> String {
    let Some(tail) = tail else {
        return lead.to_owned();
    };
    let width = UnicodeWidthStr::width(lead);
    let padding = CITATION_COLUMN.saturating_sub(width).max(2);
    format!("{lead}{}{tail}", " ".repeat(padding))
}

#[cfg(test)]
mod tests {
    use super::*;
    use code_atlas::{
        AnalysisCoverage, Claim, Evidence, EvidenceKind, NodeKey, Provenance, Section, SubjectKind,
    };
    use helix_core::Range;

    fn briefing() -> Briefing<BriefingTarget> {
        let target = BriefingTarget {
            document: DocumentId::default(),
            range: Range::new(10, 20),
        };
        let child = Claim::new(
            ClaimId::new("call"),
            "matcher.tick",
            Evidence::new(target, ":18", EvidenceKind::Call),
            Provenance::SYNTAX,
        );
        let parent = Claim::new(
            ClaimId::new("refresh"),
            "refresh_matches",
            Evidence::new(target, ":12", EvidenceKind::Definition),
            Provenance::SYNTAX,
        )
        .with_annotation("function")
        .with_child(child);
        let mut briefing = Briefing::new(
            Subject::new(NodeKey::new("file"), SubjectKind::File, "picker.rs"),
            "42 lines · 1 symbol",
            AnalysisCoverage {
                syntax_resolved: 1,
                syntax_total: 1,
                lsp_resolved: None,
                lsp_total: None,
            },
        );
        briefing.add_section(Section::new("Does the work", vec![parent]));
        briefing
    }

    #[test]
    fn briefing_starts_calm_and_keeps_mechanical_status_separate() {
        let rendered = render_briefing(&briefing(), &BTreeSet::new(), &BTreeSet::new());

        assert!(rendered
            .text
            .starts_with("picker.rs\nFile · 42 lines · 1 symbol"));
        assert!(rendered.text.contains("▸ refresh_matches"));
        assert!(!rendered.text.contains("matcher.tick"));
        assert!(rendered.text.contains("Analysis · syntax 1/1 · no lsp"));
        assert!(!rendered.text.contains('%'));
    }

    #[test]
    fn unfolding_reveals_cited_detail() {
        let rendered = render_briefing(
            &briefing(),
            &BTreeSet::from([ClaimId::new("refresh")]),
            &BTreeSet::new(),
        );

        assert!(rendered.text.contains("▾ refresh_matches"));
        assert!(rendered.text.contains("matcher.tick"));
        assert!(rendered.text.contains(":18"));
    }

    #[test]
    fn navigation_skips_non_evidence_lines() {
        let rendered = render_briefing(
            &briefing(),
            &BTreeSet::from([ClaimId::new("refresh")]),
            &BTreeSet::new(),
        );
        let first = rendered.first_actionable().unwrap();
        let second = rendered.move_from(first, 1).unwrap();

        assert_eq!(
            rendered.lines[first].claim.as_ref().unwrap().as_str(),
            "refresh"
        );
        assert_eq!(
            rendered.lines[second].claim.as_ref().unwrap().as_str(),
            "call"
        );
    }
}
