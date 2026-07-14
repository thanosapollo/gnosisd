use crate::{application::ApplicationError, domain::DocumentId};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueryDocument {
    document_id: DocumentId,
}

impl QueryDocument {
    pub fn new(document_id: impl Into<String>) -> Result<Self, ApplicationError> {
        Ok(Self {
            document_id: DocumentId::new(document_id)?,
        })
    }

    pub fn document_id(&self) -> &DocumentId {
        &self.document_id
    }
}
