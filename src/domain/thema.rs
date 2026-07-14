use super::ids::{NodeId, ThemaId};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ThemaConceptRelation {
    Primary,
    Supporting,
    Reference,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ThemaConcept {
    thema_id: ThemaId,
    concept_id: NodeId,
    relation: ThemaConceptRelation,
}

impl ThemaConcept {
    pub fn new(thema_id: ThemaId, concept_id: NodeId, relation: ThemaConceptRelation) -> Self {
        Self {
            thema_id,
            concept_id,
            relation,
        }
    }

    pub fn thema_id(&self) -> &ThemaId {
        &self.thema_id
    }
    pub fn concept_id(&self) -> &NodeId {
        &self.concept_id
    }
    pub const fn relation(&self) -> ThemaConceptRelation {
        self.relation
    }
}

#[cfg(test)]
mod tests {
    use super::{ThemaConcept, ThemaConceptRelation};
    use crate::domain::ids::{NodeId, ThemaId};

    #[test]
    fn learning_relation_is_distinct_from_graph_edges() {
        let concept = ThemaConcept::new(
            ThemaId::new("thema").unwrap(),
            NodeId::new("concept").unwrap(),
            ThemaConceptRelation::Supporting,
        );
        assert_eq!(concept.thema_id().as_str(), "thema");
        assert_eq!(concept.concept_id().as_str(), "concept");
        assert_eq!(concept.relation(), ThemaConceptRelation::Supporting);
        assert_ne!(
            ThemaConceptRelation::Primary,
            ThemaConceptRelation::Reference
        );
    }
}
