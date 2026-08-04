//! Editor- and protocol-agnostic primitives for exploring a codebase.
//!
//! [`Atlas`] folds replaceable [`Contribution`] snapshots into one evidence-backed graph.
//! A syntax adapter, language server, test index, runtime tracer, or AI agent all use the
//! same contribution contract. [`Briefing`] turns grounded claims about a real [`Subject`]
//! into an editor-neutral outline without knowing how an editor renders or opens source.
//!
//! The target type `T` belongs to the adapter. Helix uses a document-and-range anchor; another
//! editor can use an LSP location, buffer handle, URI, byte range, or any cloneable source anchor.

mod atlas;
mod briefing;
mod model;

pub use atlas::{Atlas, AtlasError, Connection, Graph, Node, Relation};
pub use briefing::{
    AnalysisCoverage, Briefing, Claim, ClaimId, ClaimTier, Evidence, EvidenceKind, OutlineLine,
    OutlineLineKind, Section, Subject, SubjectKind, Unresolved, UnresolvedReason,
};
pub use model::{
    Confidence, ConnectionDirection, Contribution, ContributorId, Fact, FactKind, GapInput,
    GapSeverity, NodeInput, NodeKey, NodeKind, Provenance, RelationInput, RelationKind, SourceKind,
};
