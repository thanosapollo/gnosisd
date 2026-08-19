use rusqlite::Transaction;

use super::{SqliteStore, StoreError};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct NotesWipeSummary {
    pub documents_removed: u64,
    pub themata_remaining: u64,
}

impl SqliteStore {
    pub(crate) fn wipe_disposable_notes(&mut self) -> Result<NotesWipeSummary, StoreError> {
        let transaction = self.write_transaction()?;
        let summary = wipe_disposable_notes_in(&transaction)?;
        transaction.commit().map_err(StoreError::storage)?;
        Ok(summary)
    }
}

fn count_rows(transaction: &Transaction<'_>, table: &str) -> Result<u64, StoreError> {
    transaction
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .map_err(StoreError::storage)
}

fn wipe_disposable_notes_in(transaction: &Transaction<'_>) -> Result<NotesWipeSummary, StoreError> {
    let documents_removed = count_rows(transaction, "document")?;
    transaction
        .execute_batch(
            "
            DELETE FROM document_revision_asset;
            DELETE FROM document_asset;
            DELETE FROM document_projection_revision;
            DELETE FROM document_revision;
            DELETE FROM retired_node_edge;
            DELETE FROM retired_node;
            DELETE FROM document;
            DELETE FROM content_change WHERE entity_kind = 'document';
            DELETE FROM thema_revision;
            DELETE FROM replica_snapshot_item;
            DELETE FROM replica_snapshot;
            ",
        )
        .map_err(StoreError::storage)?;
    Ok(NotesWipeSummary {
        documents_removed,
        themata_remaining: count_rows(transaction, "thema")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        application::{generate_device, prepare_note, prepare_thema, thema_content},
        blob::BlobMeta,
        domain::{DocumentId, ThemaConceptRelation},
    };

    #[test]
    fn wipe_drops_notes_and_keeps_themata_review_and_auth() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let device = generate_device("writer".into(), vec!["content:write".into()]).unwrap();
        let device_id = device.device_id.clone();
        store.create_device(&device, 1).unwrap();

        let note = prepare_note(
            Some("document".into()),
            "note.org".into(),
            ":PROPERTIES:\n:ID: child\n:END:\n#+title: Note\n".into(),
            None,
        )
        .unwrap();
        let blob = BlobMeta {
            sha256: note.blob_sha256.clone(),
            size_bytes: note.source.len() as u64,
            media_type: crate::parser::ORG_CONTENT_TYPE.into(),
        };
        store.replace_note(&note, &blob, 2, None).unwrap();

        let thema = prepare_thema(
            Some("thema".into()),
            thema_content(
                "basic",
                "Question".into(),
                vec![],
                vec!["Answer".into()],
                None,
                None,
                None,
            )
            .unwrap(),
            vec![],
            vec![("child".into(), ThemaConceptRelation::Primary)],
            vec![],
            None,
        )
        .unwrap();
        store.put_thema(&thema, 20260715, 3, None).unwrap();
        store.create_replica_snapshot(&device_id, 4, 16).unwrap();
        assert_eq!(table_count(&store, "replica_snapshot"), 1);
        assert!(table_count(&store, "replica_snapshot_item") > 0);

        let summary = store.wipe_disposable_notes().unwrap();
        assert_eq!(summary.documents_removed, 1);
        assert_eq!(summary.themata_remaining, 1);

        assert!(
            store
                .get_document(&DocumentId::new("document").unwrap())
                .unwrap()
                .is_none()
        );
        let stored = store.get_thema("thema").unwrap().unwrap();
        assert_eq!(stored.content.keimenon, "Question");
        assert_eq!(stored.concepts[0].concept_id().as_str(), "child");
        store.replica_state(&device_id).unwrap();
        assert_eq!(store.thema_review_state("thema").unwrap().thema_id, "thema");
        assert_eq!(table_count(&store, "replica_snapshot"), 0);
        assert_eq!(table_count(&store, "replica_snapshot_item"), 0);
    }

    fn table_count(store: &SqliteStore, table: &str) -> u64 {
        store
            .connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
    }
}
