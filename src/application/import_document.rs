use thiserror::Error;

use crate::{domain::IdError, parser::org::OrgParseError};

use super::content::ContentError;

#[cfg(test)]
use super::content::asset_paths_in_org;
#[cfg(test)]
use crate::{
    domain::{DocumentId, DocumentProjection},
    parser::org::parse_org,
};

#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportDocument {
    document_id: DocumentId,
    org: String,
    expected_revision: Option<u64>,
}

#[cfg(test)]
impl ImportDocument {
    pub fn new(
        document_id: impl Into<String>,
        org: String,
        expected_revision: Option<u64>,
    ) -> Result<Self, ApplicationError> {
        Ok(Self {
            document_id: DocumentId::new(document_id)?,
            org,
            expected_revision,
        })
    }

    pub fn prepare(self) -> Result<PreparedImport, ApplicationError> {
        Ok(self.prepare_server()?.prepared)
    }

    pub(crate) fn prepare_server(self) -> Result<PreparedServerImport, ApplicationError> {
        let projection = parse_org(&self.org)?;
        let asset_paths = asset_paths_in_org("import.org", &self.org)?;
        let prepared = PreparedImport {
            document_id: self.document_id,
            projection,
            expected_revision: self.expected_revision,
        };
        Ok(PreparedServerImport {
            prepared,
            org: self.org,
            asset_paths,
        })
    }
}

#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedImport {
    pub document_id: DocumentId,
    pub projection: DocumentProjection,
    pub expected_revision: Option<u64>,
}

#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreparedServerImport {
    pub(crate) prepared: PreparedImport,
    pub(crate) org: String,
    pub(crate) asset_paths: Vec<String>,
}

#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportSummary {
    pub document_id: DocumentId,
    pub revision: u64,
    pub content_sha256: String,
    pub node_count: usize,
    pub occurrence_count: usize,
    pub semantic_edge_count: usize,
}

#[cfg(test)]
pub fn summarize_import(prepared: &PreparedImport, revision: u64) -> ImportSummary {
    ImportSummary {
        document_id: prepared.document_id.clone(),
        revision,
        content_sha256: prepared.projection.content_sha256().to_owned(),
        node_count: prepared.projection.nodes().len(),
        occurrence_count: prepared.projection.link_occurrences().len(),
        semantic_edge_count: prepared.projection.semantic_edges().len(),
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ApplicationError {
    #[error("invalid document ID")]
    InvalidDocumentId(#[from] IdError),
    #[error("invalid Org document")]
    InvalidOrg(#[from] OrgParseError),
    #[error("invalid document content")]
    InvalidContent(#[from] ContentError),
    #[error("document revision conflict")]
    #[allow(dead_code)]
    Conflict { expected: u64, actual: Option<u64> },
    #[error("document storage failed")]
    #[allow(dead_code)]
    Storage,
}

#[cfg(test)]
mod tests {
    use super::{ApplicationError, ImportDocument};

    #[test]
    fn validates_before_preparing_an_import() {
        let error = ImportDocument::new("", "#+TITLE: Valid".into(), None).unwrap_err();
        assert!(matches!(error, ApplicationError::InvalidDocumentId(_)));
    }

    #[test]
    fn parses_before_returning_a_prepared_write() {
        let command = ImportDocument::new("doc", "no title".into(), None).unwrap();
        assert!(matches!(
            command.prepare(),
            Err(ApplicationError::InvalidOrg(_))
        ));
    }
}
