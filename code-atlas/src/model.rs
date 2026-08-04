use std::{fmt, sync::Arc};

/// Stable semantic identity supplied by an adapter.
///
/// Keys only need to be stable for the lifetime of an atlas. Adapters should map observations
/// of the same concept to the same key so independent providers compose naturally.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeKey(Arc<str>);

impl NodeKey {
    #[must_use]
    pub fn new(value: impl Into<Arc<str>>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NodeKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Identity of a replaceable provider snapshot.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContributorId(Arc<str>);

impl ContributorId {
    #[must_use]
    pub fn new(value: impl Into<Arc<str>>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NodeKind {
    Concept,
    Module,
    Type,
    Function,
    Value,
    Test,
    Boundary,
    State,
    Syntax,
    Unknown,
}

impl NodeKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Concept => "concept",
            Self::Module => "module",
            Self::Type => "type",
            Self::Function => "function",
            Self::Value => "value",
            Self::Test => "test",
            Self::Boundary => "boundary",
            Self::State => "state",
            Self::Syntax => "syntax",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RelationKind {
    Contains,
    Calls,
    DependsOn,
    Implements,
    SubtypeOf,
    Reads,
    Writes,
    Tests,
    TransitionsTo,
    Related,
}

impl RelationKind {
    #[must_use]
    pub const fn label(self, direction: ConnectionDirection) -> &'static str {
        match (self, direction) {
            (Self::Contains, ConnectionDirection::Outgoing) => "contains",
            (Self::Contains, ConnectionDirection::Incoming) => "contained by",
            (Self::Calls, ConnectionDirection::Outgoing) => "calls",
            (Self::Calls, ConnectionDirection::Incoming) => "called by",
            (Self::DependsOn, ConnectionDirection::Outgoing) => "depends on",
            (Self::DependsOn, ConnectionDirection::Incoming) => "used by",
            (Self::Implements, ConnectionDirection::Outgoing) => "implements",
            (Self::Implements, ConnectionDirection::Incoming) => "implemented by",
            (Self::SubtypeOf, ConnectionDirection::Outgoing) => "subtype of",
            (Self::SubtypeOf, ConnectionDirection::Incoming) => "supertype of",
            (Self::Reads, ConnectionDirection::Outgoing) => "reads",
            (Self::Reads, ConnectionDirection::Incoming) => "read by",
            (Self::Writes, ConnectionDirection::Outgoing) => "writes",
            (Self::Writes, ConnectionDirection::Incoming) => "written by",
            (Self::Tests, ConnectionDirection::Outgoing) => "tests",
            (Self::Tests, ConnectionDirection::Incoming) => "tested by",
            (Self::TransitionsTo, ConnectionDirection::Outgoing) => "transitions to",
            (Self::TransitionsTo, ConnectionDirection::Incoming) => "transitions from",
            (Self::Related, _) => "related to",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConnectionDirection {
    Incoming,
    Outgoing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FactKind {
    Role,
    Detail,
    Input,
    Output,
    Effect,
    Failure,
    Invariant,
    Evidence,
}

impl FactKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Role => "role",
            Self::Detail => "detail",
            Self::Input => "input",
            Self::Output => "output",
            Self::Effect => "effect",
            Self::Failure => "failure",
            Self::Invariant => "invariant",
            Self::Evidence => "evidence",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SourceKind {
    Editor,
    Syntax,
    LanguageServer,
    Test,
    Runtime,
    VersionControl,
    Agent,
    Other,
}

impl SourceKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Editor => "editor",
            Self::Syntax => "syntax",
            Self::LanguageServer => "lsp",
            Self::Test => "tests",
            Self::Runtime => "runtime",
            Self::VersionControl => "git",
            Self::Agent => "agent",
            Self::Other => "other",
        }
    }

    pub(crate) const fn authority(self) -> u8 {
        match self {
            Self::Runtime => 8,
            Self::Test => 7,
            Self::LanguageServer => 6,
            Self::Syntax => 5,
            Self::VersionControl => 4,
            Self::Editor => 3,
            Self::Other => 2,
            Self::Agent => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Confidence {
    Unknown,
    Inferred,
    Derived,
    Direct,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Provenance {
    pub source: SourceKind,
    pub confidence: Confidence,
}

impl Provenance {
    pub const SYNTAX: Self = Self::new(SourceKind::Syntax, Confidence::Direct);
    pub const LSP: Self = Self::new(SourceKind::LanguageServer, Confidence::Direct);
    pub const AGENT_INFERENCE: Self = Self::new(SourceKind::Agent, Confidence::Inferred);

    #[must_use]
    pub const fn new(source: SourceKind, confidence: Confidence) -> Self {
        Self { source, confidence }
    }

    pub(crate) const fn rank(self) -> (Confidence, u8) {
        (self.confidence, self.source.authority())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fact {
    pub kind: FactKind,
    pub text: Arc<str>,
    pub provenance: Provenance,
}

impl Fact {
    #[must_use]
    pub fn new(kind: FactKind, text: impl Into<Arc<str>>, provenance: Provenance) -> Self {
        Self {
            kind,
            text: text.into(),
            provenance,
        }
    }
}

#[derive(Clone, Debug)]
pub struct NodeInput<T> {
    pub key: NodeKey,
    pub label: Arc<str>,
    pub kind: NodeKind,
    pub detail: Option<Arc<str>>,
    pub target: Option<T>,
    pub provenance: Provenance,
    pub facts: Vec<Fact>,
}

impl<T> NodeInput<T> {
    #[must_use]
    pub fn new(
        key: NodeKey,
        label: impl Into<Arc<str>>,
        kind: NodeKind,
        provenance: Provenance,
    ) -> Self {
        Self {
            key,
            label: label.into(),
            kind,
            detail: None,
            target: None,
            provenance,
            facts: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<Arc<str>>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    #[must_use]
    pub fn with_target(mut self, target: T) -> Self {
        self.target = Some(target);
        self
    }

    #[must_use]
    pub fn with_fact(mut self, fact: Fact) -> Self {
        self.facts.push(fact);
        self
    }
}

#[derive(Clone, Debug)]
pub struct RelationInput {
    pub from: NodeKey,
    pub to: NodeKey,
    pub kind: RelationKind,
    pub provenance: Provenance,
}

impl RelationInput {
    #[must_use]
    pub const fn new(
        from: NodeKey,
        to: NodeKey,
        kind: RelationKind,
        provenance: Provenance,
    ) -> Self {
        Self {
            from,
            to,
            kind,
            provenance,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GapSeverity {
    Info,
    Warning,
    Unresolved,
}

#[derive(Clone, Debug)]
pub struct GapInput {
    pub key: Arc<str>,
    pub message: Arc<str>,
    pub severity: GapSeverity,
    pub related: Option<NodeKey>,
    pub provenance: Provenance,
}

impl GapInput {
    #[must_use]
    pub fn new(
        key: impl Into<Arc<str>>,
        message: impl Into<Arc<str>>,
        severity: GapSeverity,
        provenance: Provenance,
    ) -> Self {
        Self {
            key: key.into(),
            message: message.into(),
            severity,
            related: None,
            provenance,
        }
    }

    #[must_use]
    pub fn related_to(mut self, node: NodeKey) -> Self {
        self.related = Some(node);
        self
    }
}

/// One provider's complete, replaceable observation snapshot.
#[derive(Clone, Debug, Default)]
pub struct Contribution<T> {
    pub nodes: Vec<NodeInput<T>>,
    pub relations: Vec<RelationInput>,
    pub gaps: Vec<GapInput>,
}

impl<T> Contribution<T> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            relations: Vec::new(),
            gaps: Vec::new(),
        }
    }

    pub fn add_node(&mut self, node: NodeInput<T>) -> &mut Self {
        self.nodes.push(node);
        self
    }

    pub fn add_relation(&mut self, relation: RelationInput) -> &mut Self {
        self.relations.push(relation);
        self
    }

    pub fn add_gap(&mut self, gap: GapInput) -> &mut Self {
        self.gaps.push(gap);
        self
    }
}
