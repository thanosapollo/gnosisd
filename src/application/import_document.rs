use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    application::{PreparedNote, path_key},
    blob::BlobMeta,
    domain::{DocumentId, DocumentProjection, IdError},
    parser::org::{OrgParseError, parse_org},
    store::{SqliteStore, StoreError},
};

use super::content::{ContentError, asset_paths_in_org};

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

    #[cfg(test)]
    pub fn prepare(self) -> Result<PreparedImport, ApplicationError> {
        Ok(self.prepare_server()?.prepared)
    }

    pub(crate) fn prepare_server(self) -> Result<PreparedServerImport, ApplicationError> {
        let projection = parse_org(&self.org)?;
        let asset_paths = asset_paths_in_org(&self.org)?;
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedImport {
    pub document_id: DocumentId,
    pub projection: DocumentProjection,
    pub expected_revision: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreparedServerImport {
    pub(crate) prepared: PreparedImport,
    pub(crate) org: String,
    pub(crate) asset_paths: Vec<String>,
}

pub(crate) fn persist_server_import(
    store: &mut SqliteStore,
    server_import: &PreparedServerImport,
    blob: &BlobMeta,
    created_at_us: i64,
) -> Result<ImportSummary, ApplicationError> {
    let prepared = &server_import.prepared;
    let logical_path = store
        .document_logical_path(&prepared.document_id)
        .map_err(map_store_error)?
        .unwrap_or_else(|| protocol_logical_path(&prepared.document_id));
    let note = PreparedNote {
        document_id: prepared.document_id.clone(),
        path_key: path_key(&logical_path),
        logical_path,
        org: server_import.org.clone(),
        blob_sha256: blob.sha256.clone(),
        asset_paths: server_import.asset_paths.clone(),
        projection: prepared.projection.clone(),
        expected_revision: prepared.expected_revision,
    };
    let stored = store
        .replace_note(&note, blob, created_at_us, None)
        .map_err(map_store_error)?;
    Ok(summarize_import(prepared, stored.revision()))
}

fn protocol_logical_path(document_id: &DocumentId) -> String {
    let digest = Sha256::digest(document_id.as_str().as_bytes());
    format!("protocol/{digest:x}.org")
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

#[cfg(test)]
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
        StoreError::NotFound => ApplicationError::InvalidContent(ContentError::UnmanagedAsset),
        _ => ApplicationError::Storage,
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
    Conflict { expected: u64, actual: Option<u64> },
    #[error("document storage failed")]
    Storage,
}

#[cfg(test)]
mod tests {
    use super::{
        ApplicationError, ImportDocument, persist_import, persist_server_import,
        protocol_logical_path,
    };
    use crate::{
        application::prepare_note, blob::BlobMeta, domain::DocumentId, store::SqliteStore,
    };
    use sha2::Digest;

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
    fn protocol_import_paths_do_not_embed_document_ids() {
        let id = DocumentId::new("../nested/name").unwrap();
        let path = protocol_logical_path(&id);

        assert!(path.starts_with("protocol/"));
        assert!(path.ends_with(".org"));
        assert_eq!(path.len(), "protocol/".len() + 64 + ".org".len());
    }

    #[test]
    fn protocol_import_preserves_an_existing_logical_path() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = SqliteStore::open(directory.path().join("import.sqlite")).unwrap();
        let original = prepare_note(
            Some("document".into()),
            "notes/existing.org".into(),
            "#+TITLE: First\n".into(),
            None,
        )
        .unwrap();
        let original_blob = note_blob(&original);
        store
            .replace_note(&original, &original_blob, 1, None)
            .unwrap();
        let update = ImportDocument::new("document", "#+TITLE: Updated\n".into(), Some(1))
            .unwrap()
            .prepare_server()
            .unwrap();
        let update_blob = BlobMeta {
            sha256: format!("{:x}", sha2::Sha256::digest(update.org.as_bytes())),
            size_bytes: update.org.len() as u64,
            media_type: "text/org; charset=utf-8".into(),
        };

        let summary = persist_server_import(&mut store, &update, &update_blob, 2).unwrap();

        assert_eq!(summary.revision, 2);
        assert_eq!(
            store
                .document_logical_path(&DocumentId::new("document").unwrap())
                .unwrap()
                .as_deref(),
            Some("notes/existing.org")
        );
    }

    fn note_blob(note: &crate::application::PreparedNote) -> BlobMeta {
        BlobMeta {
            sha256: note.blob_sha256.clone(),
            size_bytes: note.org.len() as u64,
            media_type: "text/org; charset=utf-8".into(),
        }
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
