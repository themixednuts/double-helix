use std::{
    collections::{btree_map::Entry, BTreeMap, BTreeSet},
    error::Error,
    fmt,
    sync::Arc,
};

use crate::{
    ConnectionDirection, Contribution, ContributorId, Fact, GapInput, NodeInput, NodeKey, NodeKind,
    Provenance, RelationKind, SourceKind,
};

#[derive(Clone, Debug)]
pub struct Node<T> {
    pub key: NodeKey,
    pub label: Arc<str>,
    pub kind: NodeKind,
    pub detail: Option<Arc<str>>,
    pub target: Option<T>,
    pub facts: Vec<Fact>,
    pub sources: BTreeSet<SourceKind>,
    authority: Provenance,
}

#[derive(Clone, Debug)]
pub struct Relation {
    pub from: NodeKey,
    pub to: NodeKey,
    pub kind: RelationKind,
    pub provenance: Provenance,
}

#[derive(Clone, Debug)]
pub struct Connection<'a, T> {
    pub direction: ConnectionDirection,
    pub relation: &'a Relation,
    pub node: &'a Node<T>,
}

#[derive(Clone, Debug)]
pub struct Graph<T> {
    root: NodeKey,
    nodes: BTreeMap<NodeKey, Node<T>>,
    relations: Vec<Relation>,
    gaps: Vec<GapInput>,
}

impl<T> Graph<T> {
    #[must_use]
    pub fn root(&self) -> &NodeKey {
        &self.root
    }

    #[must_use]
    pub fn node(&self, key: &NodeKey) -> Option<&Node<T>> {
        self.nodes.get(key)
    }

    pub fn nodes(&self) -> impl Iterator<Item = &Node<T>> {
        self.nodes.values()
    }

    #[must_use]
    pub fn relations(&self) -> &[Relation] {
        &self.relations
    }

    #[must_use]
    pub fn gaps(&self) -> &[GapInput] {
        &self.gaps
    }

    #[must_use]
    pub fn connections(&self, key: &NodeKey) -> Vec<Connection<'_, T>> {
        self.relations
            .iter()
            .filter_map(|relation| {
                if relation.from == *key {
                    self.node(&relation.to).map(|node| Connection {
                        direction: ConnectionDirection::Outgoing,
                        relation,
                        node,
                    })
                } else if relation.to == *key {
                    self.node(&relation.from).map(|node| Connection {
                        direction: ConnectionDirection::Incoming,
                        relation,
                        node,
                    })
                } else {
                    None
                }
            })
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AtlasError {
    MissingRoot(NodeKey),
    MissingRelationEndpoint {
        contributor: ContributorId,
        endpoint: NodeKey,
    },
}

impl fmt::Display for AtlasError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingRoot(root) => write!(formatter, "atlas root `{root}` has no node"),
            Self::MissingRelationEndpoint {
                contributor,
                endpoint,
            } => write!(
                formatter,
                "contributor `{}` references missing node `{endpoint}`",
                contributor.as_str()
            ),
        }
    }
}

impl Error for AtlasError {}

/// Evidence graph assembled from independently replaceable provider snapshots.
#[derive(Clone, Debug)]
pub struct Atlas<T> {
    root: NodeKey,
    contributions: BTreeMap<ContributorId, Contribution<T>>,
    graph: Graph<T>,
}

impl<T: Clone> Atlas<T> {
    /// Creates an atlas from a validated initial provider snapshot.
    ///
    /// Requiring the root to exist at construction keeps every observable [`Graph`] valid.
    /// Additional providers can then be added or refreshed with [`Self::replace`].
    pub fn from_contribution(
        root: NodeKey,
        contributor: ContributorId,
        contribution: Contribution<T>,
    ) -> Result<Self, AtlasError> {
        let mut atlas = Self {
            graph: Graph {
                root: root.clone(),
                nodes: BTreeMap::new(),
                relations: Vec::new(),
                gaps: Vec::new(),
            },
            root,
            contributions: BTreeMap::new(),
        };
        atlas.replace(contributor, contribution)?;
        Ok(atlas)
    }

    /// Atomically replaces one provider snapshot and rebuilds the merged projection.
    ///
    /// A rejected contribution leaves the previous atlas unchanged.
    pub fn replace(
        &mut self,
        contributor: ContributorId,
        contribution: Contribution<T>,
    ) -> Result<(), AtlasError> {
        let previous = self.contributions.insert(contributor.clone(), contribution);
        match self.rebuild() {
            Ok(graph) => {
                self.graph = graph;
                Ok(())
            }
            Err(error) => {
                match previous {
                    Some(previous) => {
                        self.contributions.insert(contributor, previous);
                    }
                    None => {
                        self.contributions.remove(&contributor);
                    }
                }
                Err(error)
            }
        }
    }

    #[must_use]
    pub fn graph(&self) -> &Graph<T> {
        &self.graph
    }

    fn rebuild(&self) -> Result<Graph<T>, AtlasError> {
        let known: BTreeSet<_> = self
            .contributions
            .values()
            .flat_map(|contribution| contribution.nodes.iter().map(|node| node.key.clone()))
            .collect();
        if !known.contains(&self.root) {
            return Err(AtlasError::MissingRoot(self.root.clone()));
        }

        for (contributor, contribution) in &self.contributions {
            for relation in &contribution.relations {
                for endpoint in [&relation.from, &relation.to] {
                    if !known.contains(endpoint) {
                        return Err(AtlasError::MissingRelationEndpoint {
                            contributor: contributor.clone(),
                            endpoint: endpoint.clone(),
                        });
                    }
                }
            }
        }

        let mut nodes = BTreeMap::new();
        for input in self
            .contributions
            .values()
            .flat_map(|contribution| &contribution.nodes)
        {
            merge_node(&mut nodes, input);
        }

        let mut relations = BTreeMap::new();
        for input in self
            .contributions
            .values()
            .flat_map(|contribution| &contribution.relations)
        {
            let key = (input.from.clone(), input.kind, input.to.clone());
            match relations.entry(key) {
                Entry::Vacant(entry) => {
                    entry.insert(Relation {
                        from: input.from.clone(),
                        to: input.to.clone(),
                        kind: input.kind,
                        provenance: input.provenance,
                    });
                }
                Entry::Occupied(mut entry)
                    if input.provenance.rank() > entry.get().provenance.rank() =>
                {
                    entry.get_mut().provenance = input.provenance;
                }
                Entry::Occupied(_) => {}
            }
        }

        let mut gaps: BTreeMap<Arc<str>, GapInput> = BTreeMap::new();
        for input in self
            .contributions
            .values()
            .flat_map(|contribution| &contribution.gaps)
        {
            match gaps.entry(Arc::clone(&input.key)) {
                Entry::Vacant(entry) => {
                    entry.insert(input.clone());
                }
                Entry::Occupied(mut entry)
                    if (input.severity, input.provenance.rank())
                        > (entry.get().severity, entry.get().provenance.rank()) =>
                {
                    entry.insert(input.clone());
                }
                Entry::Occupied(_) => {}
            }
        }

        Ok(Graph {
            root: self.root.clone(),
            nodes,
            relations: relations.into_values().collect(),
            gaps: gaps.into_values().collect(),
        })
    }
}

fn merge_node<T: Clone>(nodes: &mut BTreeMap<NodeKey, Node<T>>, input: &NodeInput<T>) {
    match nodes.entry(input.key.clone()) {
        Entry::Vacant(entry) => {
            entry.insert(Node {
                key: input.key.clone(),
                label: Arc::clone(&input.label),
                kind: input.kind,
                detail: input.detail.clone(),
                target: input.target.clone(),
                facts: deduplicate_facts(input.facts.clone()),
                sources: BTreeSet::from([input.provenance.source]),
                authority: input.provenance,
            });
        }
        Entry::Occupied(mut entry) => {
            let node = entry.get_mut();
            node.sources.insert(input.provenance.source);
            if input.provenance.rank() > node.authority.rank() {
                node.label = Arc::clone(&input.label);
                node.kind = input.kind;
                if input.detail.is_some() {
                    node.detail.clone_from(&input.detail);
                }
                if input.target.is_some() {
                    node.target.clone_from(&input.target);
                }
                node.authority = input.provenance;
            } else {
                if node.detail.is_none() {
                    node.detail.clone_from(&input.detail);
                }
                if node.target.is_none() {
                    node.target.clone_from(&input.target);
                }
            }
            node.facts.extend(input.facts.iter().cloned());
            node.facts = deduplicate_facts(std::mem::take(&mut node.facts));
        }
    }
}

fn deduplicate_facts(facts: Vec<Fact>) -> Vec<Fact> {
    let mut unique: BTreeMap<_, Fact> = BTreeMap::new();
    for fact in facts {
        let key = (fact.kind, Arc::clone(&fact.text));
        match unique.entry(key) {
            Entry::Vacant(entry) => {
                entry.insert(fact);
            }
            Entry::Occupied(mut entry)
                if fact.provenance.rank() > entry.get().provenance.rank() =>
            {
                entry.insert(fact);
            }
            Entry::Occupied(_) => {}
        }
    }
    unique.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Confidence, FactKind, GapSeverity, NodeInput, RelationInput, RelationKind, SourceKind,
    };

    fn node<T>(key: &str, label: &str, provenance: Provenance) -> NodeInput<T> {
        NodeInput::new(NodeKey::new(key), label, NodeKind::Function, provenance)
    }

    #[test]
    fn replacing_contribution_removes_stale_nodes() {
        let root = NodeKey::new("root");
        let mut initial = Contribution::new();
        initial
            .add_node(node("root", "root", Provenance::SYNTAX))
            .add_node(node("old", "old", Provenance::LSP))
            .add_relation(RelationInput::new(
                root.clone(),
                NodeKey::new("old"),
                RelationKind::Calls,
                Provenance::LSP,
            ));
        let mut atlas =
            Atlas::<()>::from_contribution(root.clone(), ContributorId::new("provider"), initial)
                .unwrap();

        let mut replacement = Contribution::new();
        replacement.add_node(node("root", "root", Provenance::SYNTAX));
        atlas
            .replace(ContributorId::new("provider"), replacement)
            .unwrap();

        assert!(atlas.graph().node(&NodeKey::new("old")).is_none());
        assert!(atlas.graph().connections(&root).is_empty());
    }

    #[test]
    fn construction_rejects_a_snapshot_without_the_root() {
        let result = Atlas::<()>::from_contribution(
            NodeKey::new("root"),
            ContributorId::new("empty"),
            Contribution::new(),
        );

        assert!(matches!(result, Err(AtlasError::MissingRoot(_))));
    }

    #[test]
    fn direct_syntax_identity_beats_agent_inference_but_facts_compose() {
        let root = NodeKey::new("root");
        let mut syntax = Contribution::new();
        syntax.add_node(node("root", "parse", Provenance::SYNTAX));
        let mut atlas =
            Atlas::<()>::from_contribution(root.clone(), ContributorId::new("syntax"), syntax)
                .unwrap();

        let mut agent = Contribution::new();
        agent.add_node(
            node("root", "Maybe parser", Provenance::AGENT_INFERENCE)
                .with_detail("Domain parser boundary")
                .with_fact(Fact::new(
                    FactKind::Role,
                    "Turns source text into a syntax tree",
                    Provenance::AGENT_INFERENCE,
                )),
        );
        atlas.replace(ContributorId::new("agent"), agent).unwrap();

        let node = atlas.graph().node(&root).unwrap();
        assert_eq!(node.label.as_ref(), "parse");
        assert_eq!(node.detail.as_deref(), Some("Domain parser boundary"));
        assert_eq!(node.facts.len(), 1);
        assert_eq!(
            node.sources,
            BTreeSet::from([SourceKind::Syntax, SourceKind::Agent])
        );
    }

    #[test]
    fn rejected_replacement_preserves_previous_projection() {
        let root = NodeKey::new("root");
        let mut valid = Contribution::new();
        valid.add_node(node("root", "root", Provenance::SYNTAX));
        let mut atlas =
            Atlas::<()>::from_contribution(root.clone(), ContributorId::new("syntax"), valid)
                .unwrap();

        let mut invalid = Contribution::new();
        invalid
            .add_node(node("root", "changed", Provenance::LSP))
            .add_relation(RelationInput::new(
                root.clone(),
                NodeKey::new("missing"),
                RelationKind::Calls,
                Provenance::LSP,
            ));
        assert!(matches!(
            atlas.replace(ContributorId::new("syntax"), invalid),
            Err(AtlasError::MissingRelationEndpoint { .. })
        ));
        assert_eq!(atlas.graph().node(&root).unwrap().label.as_ref(), "root");
    }

    #[test]
    fn gaps_are_deduplicated_by_semantic_key() {
        let root = NodeKey::new("root");
        let mut contribution = Contribution::new();
        contribution
            .add_node(node("root", "root", Provenance::SYNTAX))
            .add_gap(GapInput::new(
                "dynamic",
                "dynamic edge",
                GapSeverity::Warning,
                Provenance::new(SourceKind::Syntax, Confidence::Derived),
            ))
            .add_gap(GapInput::new(
                "dynamic",
                "runtime target unresolved",
                GapSeverity::Unresolved,
                Provenance::new(SourceKind::Runtime, Confidence::Direct),
            ));
        let atlas =
            Atlas::<()>::from_contribution(root, ContributorId::new("provider"), contribution)
                .unwrap();

        assert_eq!(atlas.graph().gaps().len(), 1);
        assert_eq!(
            atlas.graph().gaps()[0].message.as_ref(),
            "runtime target unresolved"
        );
    }
}
