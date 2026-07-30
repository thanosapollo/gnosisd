use super::ids::{NodeId, ThemaId};
use thiserror::Error;

pub(crate) const MAX_REVIEW_CLOZE_INPUTS: usize = 500;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ThemaKind {
    Basic,
    Cloze,
    Mcq,
    McCloze,
}

impl ThemaKind {
    pub(crate) fn parse(value: &str) -> Result<Self, ThemaContentError> {
        match value {
            "basic" => Ok(Self::Basic),
            "cloze" => Ok(Self::Cloze),
            "mcq" => Ok(Self::Mcq),
            "mc-cloze" => Ok(Self::McCloze),
            _ => Err(ThemaContentError::Kind),
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Basic => "basic",
            Self::Cloze => "cloze",
            Self::Mcq => "mcq",
            Self::McCloze => "mc-cloze",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ThemaContent {
    pub(crate) kind: ThemaKind,
    pub(crate) keimenon: String,
    pub(crate) hypothesis: Vec<Option<String>>,
    pub(crate) answer: Vec<String>,
    pub(crate) source_guid: Option<String>,
    pub(crate) parathema: Option<String>,
    pub(crate) review_image: Option<String>,
}

impl ThemaContent {
    pub(crate) fn validate(self) -> Result<Self, ThemaContentError> {
        if self.keimenon.trim().is_empty()
            || self.answer.iter().any(|answer| answer.trim().is_empty())
        {
            return Err(ThemaContentError::Blank);
        }
        match self.kind {
            ThemaKind::Basic => validate_basic(&self)?,
            ThemaKind::Cloze => validate_cloze(&self)?,
            ThemaKind::Mcq => validate_choice(&self)?,
            ThemaKind::McCloze => validate_choice(&self)?,
        }
        Ok(self)
    }
}

fn validate_basic(content: &ThemaContent) -> Result<(), ThemaContentError> {
    if content.answer.len() == 1 && content.hypothesis.len() <= 1 {
        Ok(())
    } else {
        Err(ThemaContentError::Shape)
    }
}

fn validate_cloze(content: &ThemaContent) -> Result<(), ThemaContentError> {
    if !content.answer.is_empty()
        && content.answer.len() <= MAX_REVIEW_CLOZE_INPUTS
        && content.hypothesis.len() <= content.answer.len()
    {
        Ok(())
    } else {
        Err(ThemaContentError::Shape)
    }
}

fn validate_choice(content: &ThemaContent) -> Result<(), ThemaContentError> {
    let choices = content
        .hypothesis
        .iter()
        .map(Option::as_deref)
        .collect::<Option<Vec<_>>>()
        .ok_or(ThemaContentError::Shape)?;
    if content.answer.len() == 1
        && choices.len() >= 2
        && choices.iter().all(|choice| !choice.trim().is_empty())
        && choices.contains(&content.answer[0].as_str())
    {
        Ok(())
    } else {
        Err(ThemaContentError::Shape)
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub(crate) enum ThemaContentError {
    #[error("unsupported thema kind")]
    Kind,
    #[error("thema content contains a blank required value")]
    Blank,
    #[error("thema content does not match its kind")]
    Shape,
}

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

impl ThemaConceptRelation {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Supporting => "supporting",
            Self::Reference => "reference",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "primary" => Some(Self::Primary),
            "supporting" => Some(Self::Supporting),
            "reference" => Some(Self::Reference),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_REVIEW_CLOZE_INPUTS, ThemaConcept, ThemaConceptRelation, ThemaContent, ThemaKind,
    };
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

    #[test]
    fn cloze_content_bounds_review_inputs() {
        let content = |count| ThemaContent {
            kind: ThemaKind::Cloze,
            keimenon: "question".into(),
            hypothesis: vec![],
            answer: vec!["answer".into(); count],
            source_guid: None,
            parathema: None,
            review_image: None,
        };

        assert!(content(MAX_REVIEW_CLOZE_INPUTS).validate().is_ok());
        assert!(content(MAX_REVIEW_CLOZE_INPUTS + 1).validate().is_err());
    }
}
