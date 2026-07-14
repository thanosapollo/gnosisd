use thiserror::Error;

use crate::{
    domain::{DocumentId, DocumentProjection, IdError},
    parser::org::{OrgParseError, parse_org},
    store::{SqliteStore, StoreError},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportDocument {
    document_id: DocumentId,
    org: String,
    expected_revision: Option<u64>,
}

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
        let projection = parse_org(&self.org)?;
        Ok(PreparedImport {
            document_id: self.document_id,
            projection,
            expected_revision: self.expected_revision,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedImport {
    pub document_id: DocumentId,
    pub projection: DocumentProjection,
    pub expected_revision: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportSummary {
    pub document_id: DocumentId,
    pub revision: u64,
    pub content_sha256: String,
    pub node_count: usize,
    pub occurrence_count: usize,
    pub semantic_edge_count: usize,
}

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

pub(crate) fn persist_import(
    store: &mut SqliteStore,
    prepared: &PreparedImport,
) -> Result<ImportSummary, ApplicationError> {
    let stored = store
        .replace_document(
            &prepared.document_id,
            &prepared.projection,
            prepared.expected_revision,
        )
        .map_err(map_store_error)?;
    Ok(summarize_import(prepared, stored.revision()))
}

fn map_store_error(error: StoreError) -> ApplicationError {
    match error {
        StoreError::Conflict { expected, actual } => {
            ApplicationError::Conflict { expected, actual }
        }
        _ => ApplicationError::Storage,
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ApplicationError {
    #[error("invalid document ID")]
    InvalidDocumentId(#[from] IdError),
    #[error("invalid Org document")]
    InvalidOrg(#[from] OrgParseError),
    #[error("document revision conflict")]
    Conflict { expected: u64, actual: Option<u64> },
    #[error("document storage failed")]
    Storage,
}

#[cfg(test)]
mod tests {
    use super::{ApplicationError, ImportDocument, persist_import};
    use crate::store::SqliteStore;

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

    #[test]
    fn application_service_reports_stale_revision_from_temporary_sqlite() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = SqliteStore::open(directory.path().join("application.sqlite")).unwrap();
        let first = ImportDocument::new("document", "#+TITLE: First\n".into(), None)
            .unwrap()
            .prepare()
            .unwrap();
        assert_eq!(persist_import(&mut store, &first).unwrap().revision, 1);
        let stale = ImportDocument::new("document", "#+TITLE: Second\n".into(), Some(0))
            .unwrap()
            .prepare()
            .unwrap();
        assert!(matches!(
            persist_import(&mut store, &stale),
            Err(ApplicationError::Conflict {
                expected: 0,
                actual: Some(1)
            })
        ));
    }
}
