use sha2::{Digest, Sha256};

use crate::domain::DocumentId;

#[cfg(test)]
use crate::{
    application::{
        ApplicationError, ContentError, ImportSummary, PreparedNote, PreparedServerImport,
        path_key, summarize_import,
    },
    blob::BlobMeta,
    store::{SqliteStore, StoreError},
};

#[cfg(test)]
pub(super) fn persist_server_import(
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
        explicit_document_id: true,
        logical_path_is_request_input: false,
        path_key: path_key(&logical_path),
        logical_path,
        content_type: "text/org; charset=utf-8".into(),
        source: server_import.org.clone(),
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

pub(crate) fn protocol_logical_path(document_id: &DocumentId) -> String {
    let digest = Sha256::digest(document_id.as_str().as_bytes());
    crate::parser::generated_document_path(
        crate::parser::ORG_CONTENT_TYPE,
        &format!("protocol/{digest:x}"),
    )
    .expect("the built-in document adapter must accept generated paths")
}

#[cfg(test)]
fn map_store_error(error: StoreError) -> ApplicationError {
    match error {
        StoreError::Conflict { expected, actual } => {
            ApplicationError::Conflict { expected, actual }
        }
        StoreError::NotFound => ApplicationError::InvalidContent(ContentError::UnmanagedAsset),
        _ => ApplicationError::Storage,
    }
}

#[cfg(test)]
mod tests {
    use sha2::Digest as _;

    use super::{persist_server_import, protocol_logical_path};
    use crate::{
        application::{ApplicationError, ImportDocument, PreparedServerImport, prepare_note},
        blob::BlobMeta,
        domain::DocumentId,
        store::SqliteStore,
    };

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
        let update = prepare_import("#+TITLE: Updated\n", Some(1));

        let summary = persist_server_import(&mut store, &update, &import_blob(&update), 2).unwrap();

        assert_eq!(summary.revision, 2);
        assert_eq!(
            store
                .document_logical_path(&DocumentId::new("document").unwrap())
                .unwrap()
                .as_deref(),
            Some("notes/existing.org")
        );
    }

    #[test]
    fn stale_revision_maps_to_application_conflict() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = SqliteStore::open(directory.path().join("import.sqlite")).unwrap();
        let first = prepare_import("#+TITLE: First\n", None);
        persist_server_import(&mut store, &first, &import_blob(&first), 1).unwrap();
        let stale = prepare_import("#+TITLE: Second\n", Some(0));

        assert!(matches!(
            persist_server_import(&mut store, &stale, &import_blob(&stale), 2),
            Err(ApplicationError::Conflict {
                expected: 0,
                actual: Some(1)
            })
        ));
    }

    fn prepare_import(org: &str, expected_revision: Option<u64>) -> PreparedServerImport {
        ImportDocument::new("document", org.into(), expected_revision)
            .unwrap()
            .prepare_server()
            .unwrap()
    }

    fn import_blob(import: &PreparedServerImport) -> BlobMeta {
        BlobMeta {
            sha256: format!("{:x}", sha2::Sha256::digest(import.org.as_bytes())),
            size_bytes: import.org.len() as u64,
            media_type: "text/org; charset=utf-8".into(),
        }
    }

    fn note_blob(note: &crate::application::PreparedNote) -> BlobMeta {
        BlobMeta {
            sha256: note.blob_sha256.clone(),
            size_bytes: note.source.len() as u64,
            media_type: "text/org; charset=utf-8".into(),
        }
    }
}
