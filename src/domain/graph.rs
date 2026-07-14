use std::collections::HashSet;

use super::{LinkOccurrence, NodeId};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct NodeEdge {
    source: NodeId,
    target: NodeId,
}

impl NodeEdge {
    pub fn new(source: NodeId, target: NodeId) -> Self {
        Self { source, target }
    }

    pub fn source(&self) -> &NodeId {
        &self.source
    }

    pub fn target(&self) -> &NodeId {
        &self.target
    }
}

pub fn project_unique_edges(occurrences: &[LinkOccurrence]) -> Vec<NodeEdge> {
    occurrences
        .iter()
        .map(|occurrence| NodeEdge::new(occurrence.owner().clone(), occurrence.target().clone()))
        .scan(HashSet::new(), |seen, edge| {
            Some(seen.insert(edge.clone()).then_some(edge))
        })
        .flatten()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::project_unique_edges;
    use crate::domain::{LinkOccurrence, NodeId, SourceSpan};

    #[test]
    fn duplicate_occurrences_produce_one_semantic_edge() {
        let owner = NodeId::new("owner").unwrap();
        let target = NodeId::new("target").unwrap();
        let occurrences = vec![
            LinkOccurrence::new(
                owner.clone(),
                target.clone(),
                SourceSpan::for_source("  xx      ", 2, 4).unwrap(),
            ),
            LinkOccurrence::new(
                owner,
                target,
                SourceSpan::for_source("        xx", 8, 10).unwrap(),
            ),
        ];

        assert_eq!(project_unique_edges(&occurrences).len(), 1);
    }
}
