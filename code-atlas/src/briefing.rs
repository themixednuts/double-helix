use std::{collections::BTreeSet, fmt, sync::Arc};

use crate::{NodeKey, Provenance};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClaimId(Arc<str>);

impl ClaimId {
    #[must_use]
    pub fn new(value: impl Into<Arc<str>>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ClaimId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubjectKind {
    Symbol,
    File,
    Directory,
    Crate,
    Diff,
    Workspace,
}

impl SubjectKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Symbol => "Symbol",
            Self::File => "File",
            Self::Directory => "Directory",
            Self::Crate => "Crate",
            Self::Diff => "Diff",
            Self::Workspace => "Workspace",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subject {
    pub key: NodeKey,
    pub kind: SubjectKind,
    pub label: Arc<str>,
}

impl Subject {
    #[must_use]
    pub fn new(key: NodeKey, kind: SubjectKind, label: impl Into<Arc<str>>) -> Self {
        Self {
            key,
            kind,
            label: label.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvidenceKind {
    Definition,
    Reference,
    Call,
    Effect,
    Boundary,
    Candidate,
}

#[derive(Clone, Debug)]
pub struct Evidence<T> {
    pub target: T,
    pub label: Arc<str>,
    pub kind: EvidenceKind,
}

impl<T> Evidence<T> {
    #[must_use]
    pub fn new(target: T, label: impl Into<Arc<str>>, kind: EvidenceKind) -> Self {
        Self {
            target,
            label: label.into(),
            kind,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimTier {
    Mechanical,
    Rewritten,
    Inferred,
}

#[derive(Clone, Debug)]
pub struct Claim<T> {
    pub id: ClaimId,
    pub lead: Arc<str>,
    pub annotation: Option<Arc<str>>,
    pub evidence: Vec<Evidence<T>>,
    pub tier: ClaimTier,
    pub provenance: Provenance,
    pub zoom: Option<Subject>,
    pub children: Vec<Claim<T>>,
}

impl<T> Claim<T> {
    /// Creates a claim with its required first citation.
    #[must_use]
    pub fn new(
        id: ClaimId,
        lead: impl Into<Arc<str>>,
        evidence: Evidence<T>,
        provenance: Provenance,
    ) -> Self {
        Self {
            id,
            lead: lead.into(),
            annotation: None,
            evidence: vec![evidence],
            tier: ClaimTier::Mechanical,
            provenance,
            zoom: None,
            children: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_annotation(mut self, annotation: impl Into<Arc<str>>) -> Self {
        self.annotation = Some(annotation.into());
        self
    }

    #[must_use]
    pub fn with_zoom(mut self, subject: Subject) -> Self {
        self.zoom = Some(subject);
        self
    }

    #[must_use]
    pub fn with_child(mut self, child: Claim<T>) -> Self {
        self.children.push(child);
        self
    }

    #[must_use]
    pub fn with_children(mut self, children: impl IntoIterator<Item = Claim<T>>) -> Self {
        self.children.extend(children);
        self
    }

    #[must_use]
    pub fn with_tier(mut self, tier: ClaimTier) -> Self {
        self.tier = tier;
        self
    }
}

#[derive(Clone, Debug)]
pub struct Section<T> {
    pub title: Arc<str>,
    pub claims: Vec<Claim<T>>,
}

impl<T> Section<T> {
    #[must_use]
    pub fn new(title: impl Into<Arc<str>>, claims: Vec<Claim<T>>) -> Self {
        Self {
            title: title.into(),
            claims,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnresolvedReason {
    DynamicDispatch,
    MacroExpansion,
    GeneratedCode,
    Reflection,
    MissingRuntimeEvidence,
    ToolUnavailable,
    UnsupportedStructure,
    Unreferenced,
}

impl UnresolvedReason {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::DynamicDispatch => "dynamic dispatch",
            Self::MacroExpansion => "macro expansion",
            Self::GeneratedCode => "generated code",
            Self::Reflection => "reflection",
            Self::MissingRuntimeEvidence => "no runtime evidence",
            Self::ToolUnavailable => "tool unavailable",
            Self::UnsupportedStructure => "unsupported structure",
            Self::Unreferenced => "no local reference",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Unresolved<T> {
    pub what: Arc<str>,
    pub reason: UnresolvedReason,
    pub candidates: Vec<Evidence<T>>,
}

impl<T> Unresolved<T> {
    #[must_use]
    pub fn new(what: impl Into<Arc<str>>, reason: UnresolvedReason) -> Self {
        Self {
            what: what.into(),
            reason,
            candidates: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_candidate(mut self, candidate: Evidence<T>) -> Self {
        self.candidates.push(candidate);
        self
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AnalysisCoverage {
    pub syntax_resolved: usize,
    pub syntax_total: usize,
    pub lsp_resolved: Option<usize>,
    pub lsp_total: Option<usize>,
}

impl AnalysisCoverage {
    #[must_use]
    pub fn label(self) -> String {
        let syntax = format!("syntax {}/{}", self.syntax_resolved, self.syntax_total);
        match (self.lsp_resolved, self.lsp_total) {
            (Some(resolved), Some(total)) => format!("{syntax} · lsp {resolved}/{total}"),
            _ => format!("{syntax} · no lsp"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Briefing<T> {
    pub subject: Subject,
    pub summary: Arc<str>,
    pub sections: Vec<Section<T>>,
    pub unresolved: Vec<Unresolved<T>>,
    pub coverage: AnalysisCoverage,
}

impl<T> Briefing<T> {
    #[must_use]
    pub fn new(subject: Subject, summary: impl Into<Arc<str>>, coverage: AnalysisCoverage) -> Self {
        Self {
            subject,
            summary: summary.into(),
            sections: Vec::new(),
            unresolved: Vec::new(),
            coverage,
        }
    }

    pub fn add_section(&mut self, section: Section<T>) -> &mut Self {
        if !section.claims.is_empty() {
            self.sections.push(section);
        }
        self
    }

    pub fn add_unresolved(&mut self, unresolved: Unresolved<T>) -> &mut Self {
        self.unresolved.push(unresolved);
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutlineLineKind {
    Subject,
    Summary,
    Section,
    Claim,
    Unknown,
    Status,
}

#[derive(Clone, Debug)]
pub struct OutlineLine<T> {
    pub kind: OutlineLineKind,
    pub depth: u8,
    pub lead: Arc<str>,
    pub tail: Option<Arc<str>>,
    pub claim: Option<ClaimId>,
    pub target: Option<T>,
    pub zoom: Option<Subject>,
    pub foldable: bool,
    pub expanded: bool,
}

impl<T: Clone> Briefing<T> {
    #[must_use]
    pub fn outline(&self, expanded: &BTreeSet<ClaimId>) -> Vec<OutlineLine<T>> {
        let mut lines = vec![
            OutlineLine {
                kind: OutlineLineKind::Subject,
                depth: 0,
                lead: Arc::clone(&self.subject.label),
                tail: Some(Arc::from(self.subject.kind.label())),
                claim: None,
                target: None,
                zoom: None,
                foldable: false,
                expanded: false,
            },
            OutlineLine {
                kind: OutlineLineKind::Summary,
                depth: 0,
                lead: Arc::clone(&self.summary),
                tail: None,
                claim: None,
                target: None,
                zoom: None,
                foldable: false,
                expanded: false,
            },
        ];

        for section in &self.sections {
            lines.push(OutlineLine {
                kind: OutlineLineKind::Section,
                depth: 0,
                lead: Arc::clone(&section.title),
                tail: None,
                claim: None,
                target: None,
                zoom: None,
                foldable: false,
                expanded: false,
            });
            for claim in &section.claims {
                push_claim(&mut lines, claim, 1, expanded);
            }
        }

        if !self.unresolved.is_empty() {
            lines.push(OutlineLine {
                kind: OutlineLineKind::Section,
                depth: 0,
                lead: Arc::from("Can't resolve"),
                tail: None,
                claim: None,
                target: None,
                zoom: None,
                foldable: false,
                expanded: false,
            });
            lines.extend(self.unresolved.iter().map(|unknown| {
                OutlineLine {
                    kind: OutlineLineKind::Unknown,
                    depth: 1,
                    lead: Arc::clone(&unknown.what),
                    tail: Some(Arc::from(unknown.reason.label())),
                    claim: None,
                    target: unknown
                        .candidates
                        .first()
                        .map(|candidate| candidate.target.clone()),
                    zoom: None,
                    foldable: false,
                    expanded: false,
                }
            }));
        }

        lines.push(OutlineLine {
            kind: OutlineLineKind::Status,
            depth: 0,
            lead: Arc::from(self.coverage.label()),
            tail: None,
            claim: None,
            target: None,
            zoom: None,
            foldable: false,
            expanded: false,
        });
        lines
    }
}

fn push_claim<T: Clone>(
    lines: &mut Vec<OutlineLine<T>>,
    claim: &Claim<T>,
    depth: u8,
    expanded: &BTreeSet<ClaimId>,
) {
    let is_expanded = expanded.contains(&claim.id);
    let citation = &claim.evidence[0];
    let tail = match &claim.annotation {
        Some(annotation) => Arc::from(format!("{} · {annotation}", citation.label)),
        None => Arc::clone(&citation.label),
    };
    lines.push(OutlineLine {
        kind: OutlineLineKind::Claim,
        depth,
        lead: Arc::clone(&claim.lead),
        tail: Some(tail),
        claim: Some(claim.id.clone()),
        target: Some(citation.target.clone()),
        zoom: claim.zoom.clone(),
        foldable: !claim.children.is_empty(),
        expanded: is_expanded,
    });
    if is_expanded {
        for child in &claim.children {
            push_claim(lines, child, depth.saturating_add(1), expanded);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Provenance;

    fn evidence(line: usize) -> Evidence<usize> {
        Evidence::new(line, format!(":{line}"), EvidenceKind::Definition)
    }

    #[test]
    fn a_claim_is_constructed_with_grounding() {
        let claim = Claim::new(
            ClaimId::new("refresh"),
            "refresh_matches",
            evidence(12),
            Provenance::SYNTAX,
        );

        assert_eq!(claim.evidence.len(), 1);
        assert_eq!(claim.evidence[0].target, 12);
    }

    #[test]
    fn children_are_progressively_disclosed() {
        let parent = Claim::new(
            ClaimId::new("refresh"),
            "refresh_matches",
            evidence(12),
            Provenance::SYNTAX,
        )
        .with_child(Claim::new(
            ClaimId::new("tick"),
            "matcher.tick",
            evidence(18),
            Provenance::SYNTAX,
        ));
        let subject = Subject::new(NodeKey::new("file"), SubjectKind::File, "picker.rs");
        let mut briefing = Briefing::new(subject, "2 symbols", AnalysisCoverage::default());
        briefing.add_section(Section::new("Does the work", vec![parent]));

        let folded = briefing.outline(&BTreeSet::new());
        assert!(!folded
            .iter()
            .any(|line| line.lead.as_ref() == "matcher.tick"));

        let expanded = briefing.outline(&BTreeSet::from([ClaimId::new("refresh")]));
        assert!(expanded
            .iter()
            .any(|line| line.lead.as_ref() == "matcher.tick"));
    }

    #[test]
    fn coverage_reports_tools_without_claiming_comprehension() {
        assert_eq!(
            AnalysisCoverage {
                syntax_resolved: 8,
                syntax_total: 9,
                lsp_resolved: None,
                lsp_total: None,
            }
            .label(),
            "syntax 8/9 · no lsp"
        );
    }
}
