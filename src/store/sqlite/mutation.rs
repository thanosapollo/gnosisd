use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use super::{
    NoteWriteDecision, SqliteStore, StoreError, current_content_cursor, load_document_head,
    note_write_decision, replace_note_in, replica_cursor_token, server_id,
};
use crate::{
    application::{
        DeviceMutationCursor, DeviceMutationIdentity, DeviceMutationKind, DeviceMutationProjection,
        DeviceMutationRejection, DeviceMutationResult, PreparedDeviceDocumentMutation,
    },
    blob::BlobMeta,
};

const REPLAY_SQL: &str = "SELECT request_sha256, request_json, result_kind, result_json
     FROM device_mutation_receipt WHERE device_id = ?1 AND operation_id = ?2";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DeviceMutationExecution {
    pub(crate) result: DeviceMutationResult,
    pub(crate) replayed: bool,
}

impl SqliteStore {
    pub(crate) fn apply_device_document_mutation(
        &mut self,
        operation: &PreparedDeviceDocumentMutation,
        blob: Option<&BlobMeta>,
        created_at_us: i64,
    ) -> Result<DeviceMutationExecution, StoreError> {
        validate_document_operation(operation, blob)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(StoreError::storage)?;
        if let Some(replay) = replayed_device_mutation(&transaction, &operation.identity)? {
            transaction.commit().map_err(StoreError::storage)?;
            return Ok(replay);
        }
        let result = apply_new_document_mutation(&transaction, operation, blob, created_at_us)?;
        insert_receipt(&transaction, &operation.identity, &result, created_at_us)?;
        transaction.commit().map_err(StoreError::storage)?;
        Ok(DeviceMutationExecution {
            result,
            replayed: false,
        })
    }

    pub(crate) fn device_document_mutation_needs_blob(
        &self,
        operation: &PreparedDeviceDocumentMutation,
    ) -> Result<bool, StoreError> {
        validate_document_operation(operation, None)?;
        if replayed_device_mutation(&self.connection, &operation.identity)?.is_some() {
            return Ok(false);
        }
        Ok(matches!(
            document_mutation_preflight(&self.connection, operation)?,
            DocumentMutationPreflight::Write
        ))
    }
}

fn validate_document_operation(
    operation: &PreparedDeviceDocumentMutation,
    blob: Option<&BlobMeta>,
) -> Result<(), StoreError> {
    let prepared = &operation.prepared;
    let digest = format!("{:x}", Sha256::digest(prepared.org.as_bytes()));
    let blob_matches = blob.is_none_or(|blob| {
        blob.sha256 == digest
            && blob.size_bytes == prepared.org.len() as u64
            && blob.media_type == "text/org; charset=utf-8"
    });
    if operation.validate().is_ok() && prepared.blob_sha256 == digest && blob_matches {
        Ok(())
    } else {
        Err(StoreError::CorruptData {
            field: "device document mutation",
        })
    }
}

fn replayed_device_mutation(
    connection: &Connection,
    identity: &DeviceMutationIdentity,
) -> Result<Option<DeviceMutationExecution>, StoreError> {
    let stored = connection
        .query_row(
            REPLAY_SQL,
            params![identity.device_id, identity.operation_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()
        .map_err(StoreError::storage)?;
    let Some((digest, request_json, result_kind, result_json)) = stored else {
        return Ok(None);
    };
    verify_replayed_request(identity, &digest, &request_json)?;
    let result = decode_replayed_result(identity, &result_kind, &result_json)?;
    Ok(Some(DeviceMutationExecution {
        result,
        replayed: true,
    }))
}

fn verify_replayed_request(
    identity: &DeviceMutationIdentity,
    stored_digest: &str,
    stored_json: &str,
) -> Result<(), StoreError> {
    let actual = format!("{:x}", Sha256::digest(stored_json.as_bytes()));
    if actual != stored_digest {
        return Err(StoreError::CorruptData {
            field: "device mutation request",
        });
    }
    if stored_digest == identity.request_sha256 && stored_json == identity.request_json {
        Ok(())
    } else {
        Err(StoreError::IdempotencyConflict)
    }
}

fn decode_replayed_result(
    identity: &DeviceMutationIdentity,
    stored_kind: &str,
    stored_json: &str,
) -> Result<DeviceMutationResult, StoreError> {
    let result: DeviceMutationResult =
        serde_json::from_str(stored_json).map_err(|_| StoreError::CorruptData {
            field: "device mutation result",
        })?;
    if result.result_kind() != stored_kind || result.validate_for(identity).is_err() {
        Err(StoreError::CorruptData {
            field: "device mutation result",
        })
    } else {
        Ok(result)
    }
}

fn apply_new_document_mutation(
    transaction: &Transaction<'_>,
    operation: &PreparedDeviceDocumentMutation,
    blob: Option<&BlobMeta>,
    created_at_us: i64,
) -> Result<DeviceMutationResult, StoreError> {
    let before = load_document_head(transaction, &operation.prepared.document_id)?;
    match document_mutation_preflight(transaction, operation)? {
        DocumentMutationPreflight::Rejected(rejection) => {
            return rejected_result(transaction, &operation.identity, rejection);
        }
        DocumentMutationPreflight::Unchanged => {
            return unchanged_document_result(transaction, operation);
        }
        DocumentMutationPreflight::Write => {}
    }
    let blob = blob.ok_or(StoreError::CorruptData {
        field: "device document blob",
    })?;
    transaction
        .execute_batch("SAVEPOINT device_mutation_apply")
        .map_err(StoreError::storage)?;
    match replace_note_in(
        transaction,
        &operation.prepared,
        blob,
        operation.prepared.expected_revision,
        created_at_us,
        Some(&operation.identity.device_id),
        true,
    ) {
        Ok(_) => applied_document_result(transaction, operation, before),
        Err(error) => rejected_document_result(transaction, operation, error),
    }
}

enum DocumentMutationPreflight {
    Rejected(DeviceMutationRejection),
    Unchanged,
    Write,
}

fn document_mutation_preflight(
    connection: &Connection,
    operation: &PreparedDeviceDocumentMutation,
) -> Result<DocumentMutationPreflight, StoreError> {
    let before = load_document_head(connection, &operation.prepared.document_id)?;
    if operation.identity.mutation_kind == DeviceMutationKind::DocumentCreate && before.is_some() {
        return Ok(DocumentMutationPreflight::Rejected(
            DeviceMutationRejection::RevisionConflict {
                expected: None,
                actual: before.map(|head| head.revision),
            },
        ));
    }
    match note_write_decision(
        connection,
        &operation.prepared,
        operation.prepared.expected_revision,
        true,
    ) {
        Ok(NoteWriteDecision::Unchanged(_)) => Ok(DocumentMutationPreflight::Unchanged),
        Ok(NoteWriteDecision::Write(_)) => Ok(DocumentMutationPreflight::Write),
        Err(error) => match document_rejection(&error) {
            Some(rejection) => Ok(DocumentMutationPreflight::Rejected(rejection)),
            None => Err(error),
        },
    }
}

fn unchanged_document_result(
    connection: &Connection,
    operation: &PreparedDeviceDocumentMutation,
) -> Result<DeviceMutationResult, StoreError> {
    let head = load_document_head(connection, &operation.prepared.document_id)?
        .ok_or(StoreError::NotFound)?;
    Ok(DeviceMutationResult::applied(
        &operation.identity,
        false,
        DeviceMutationProjection::Document(head),
        mutation_cursor(connection)?,
    ))
}

fn applied_document_result(
    transaction: &Transaction<'_>,
    operation: &PreparedDeviceDocumentMutation,
    before: Option<crate::application::DocumentHead>,
) -> Result<DeviceMutationResult, StoreError> {
    transaction
        .execute_batch("RELEASE device_mutation_apply")
        .map_err(StoreError::storage)?;
    let head = load_document_head(transaction, &operation.prepared.document_id)?
        .ok_or(StoreError::NotFound)?;
    let changed = before.as_ref() != Some(&head);
    Ok(DeviceMutationResult::applied(
        &operation.identity,
        changed,
        DeviceMutationProjection::Document(head),
        mutation_cursor(transaction)?,
    ))
}

fn rejected_document_result(
    transaction: &Transaction<'_>,
    operation: &PreparedDeviceDocumentMutation,
    error: StoreError,
) -> Result<DeviceMutationResult, StoreError> {
    let Some(rejection) = document_rejection(&error) else {
        return Err(error);
    };
    transaction
        .execute_batch("ROLLBACK TO device_mutation_apply; RELEASE device_mutation_apply")
        .map_err(StoreError::storage)?;
    rejected_result(transaction, &operation.identity, rejection)
}

fn document_rejection(error: &StoreError) -> Option<DeviceMutationRejection> {
    match error {
        StoreError::Conflict { expected, actual } => {
            Some(DeviceMutationRejection::RevisionConflict {
                expected: Some(*expected),
                actual: *actual,
            })
        }
        StoreError::NotFound => Some(DeviceMutationRejection::InvalidContent),
        StoreError::InvalidProjection { .. } => Some(DeviceMutationRejection::InvalidContent),
        StoreError::PathChange => Some(DeviceMutationRejection::RenameRequired),
        _ => None,
    }
}

fn rejected_result(
    connection: &Connection,
    identity: &DeviceMutationIdentity,
    rejection: DeviceMutationRejection,
) -> Result<DeviceMutationResult, StoreError> {
    Ok(DeviceMutationResult::rejected(
        identity,
        rejection,
        mutation_cursor(connection)?,
    ))
}

fn mutation_cursor(connection: &Connection) -> Result<DeviceMutationCursor, StoreError> {
    let position = current_content_cursor(connection)?;
    Ok(DeviceMutationCursor {
        server_id: server_id(connection)?,
        position,
        token: replica_cursor_token(connection, position)?,
    })
}

fn insert_receipt(
    transaction: &Transaction<'_>,
    identity: &DeviceMutationIdentity,
    result: &DeviceMutationResult,
    created_at_us: i64,
) -> Result<(), StoreError> {
    let result_json = serde_json::to_string(result).map_err(StoreError::storage)?;
    transaction
        .execute(
            "INSERT INTO device_mutation_receipt(
                 device_id, operation_id, format_version, mutation_kind, request_sha256,
                 request_json, result_kind, result_json, created_at_us
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                identity.device_id,
                identity.operation_id,
                identity.format_version,
                identity.mutation_kind.as_str(),
                identity.request_sha256,
                identity.request_json,
                result.result_kind(),
                result_json,
                created_at_us,
            ],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};
    use tempfile::tempdir;

    use super::super::{SqliteStore, StoreError};
    use crate::{
        application::{
            DeviceMutationProjection, DeviceMutationRejection, DeviceMutationResult,
            PreparedDeviceDocumentMutation, generate_device, prepare_device_document_mutation,
            prepare_note,
        },
        blob::BlobMeta,
    };

    #[test]
    fn document_mutation_replays_and_rejects_changed_key_reuse() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let device = add_device(&mut store, "first");
        let first = operation(&device, "operation", "document", "note.org", "First", None);
        let applied = store
            .apply_device_document_mutation(&first, Some(&blob(&first)), 2)
            .unwrap();
        let retry = operation(&device, "operation", "other-id", "note.org", "First", None);
        let replay = store
            .apply_device_document_mutation(&retry, Some(&blob(&retry)), 3)
            .unwrap();
        let changed = operation(
            &device,
            "operation",
            "third-id",
            "note.org",
            "Changed",
            None,
        );

        assert!(!applied.replayed);
        assert!(replay.replayed);
        assert_eq!(replay.result, applied.result);
        assert_applied_document(&applied.result, "document", 1, true);
        assert!(matches!(
            store.apply_device_document_mutation(&changed, Some(&blob(&changed)), 4),
            Err(StoreError::IdempotencyConflict)
        ));
        assert_eq!(row_count(&store, "document_revision"), 1);
        assert_eq!(row_count(&store, "device_mutation_receipt"), 1);
    }

    #[test]
    fn document_mutation_identity_is_scoped_to_the_device() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let first_device = add_device(&mut store, "first");
        let second_device = add_device(&mut store, "second");
        let first = operation(
            &first_device,
            "shared",
            "first-document",
            "first.org",
            "First",
            None,
        );
        let second = operation(
            &second_device,
            "shared",
            "second-document",
            "second.org",
            "Second",
            None,
        );

        store
            .apply_device_document_mutation(&first, Some(&blob(&first)), 3)
            .unwrap();
        let result = store
            .apply_device_document_mutation(&second, Some(&blob(&second)), 4)
            .unwrap();

        assert_applied_document(&result.result, "second-document", 1, true);
        assert_eq!(row_count(&store, "device_mutation_receipt"), 2);
    }

    #[test]
    fn document_mutation_replay_survives_restart() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("gnosis.sqlite");
        let (operation, first) = {
            let mut store = SqliteStore::open(&path).unwrap();
            let device = add_device(&mut store, "device");
            let operation = operation(&device, "restart", "document", "note.org", "Note", None);
            let first = store
                .apply_device_document_mutation(&operation, Some(&blob(&operation)), 2)
                .unwrap();
            (operation, first)
        };
        let mut reopened = SqliteStore::open(path).unwrap();

        let replay = reopened
            .apply_device_document_mutation(&operation, Some(&blob(&operation)), 3)
            .unwrap();

        assert!(replay.replayed);
        assert_eq!(replay.result, first.result);
        assert_eq!(row_count(&reopened, "document_revision"), 1);
    }

    #[test]
    fn stale_document_result_is_retained_after_later_canonical_change() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let device = add_device(&mut store, "device");
        let create = operation(&device, "create", "document", "note.org", "First", None);
        store
            .apply_device_document_mutation(&create, Some(&blob(&create)), 2)
            .unwrap();
        let stale = operation(&device, "stale", "document", "note.org", "Stale", Some(0));
        let conflict = store
            .apply_device_document_mutation(&stale, Some(&blob(&stale)), 3)
            .unwrap();
        let update = operation(
            &device,
            "update",
            "document",
            "note.org",
            "Current",
            Some(1),
        );
        store
            .apply_device_document_mutation(&update, Some(&blob(&update)), 4)
            .unwrap();
        let replay = store
            .apply_device_document_mutation(&stale, Some(&blob(&stale)), 5)
            .unwrap();

        assert_rejected_revision(&conflict.result, Some(0), Some(1), 1);
        assert!(replay.replayed);
        assert_eq!(replay.result, conflict.result);
        assert_eq!(
            store
                .document_head(&update.prepared.document_id)
                .unwrap()
                .unwrap()
                .revision,
            2
        );
    }

    #[test]
    fn failed_receipt_insert_rolls_back_document_and_feed() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let operation = operation(
            "missing-device",
            "rollback",
            "document",
            "note.org",
            "Note",
            None,
        );

        assert!(matches!(
            store.apply_device_document_mutation(&operation, Some(&blob(&operation)), 1),
            Err(StoreError::Storage(_))
        ));
        assert_eq!(row_count(&store, "document"), 0);
        assert_eq!(row_count(&store, "document_revision"), 0);
        assert_eq!(row_count(&store, "content_change"), 0);
        assert_eq!(row_count(&store, "blob"), 0);
    }

    fn add_device(store: &mut SqliteStore, name: &str) -> String {
        let device = generate_device(name.into(), vec!["content:write".into()]).unwrap();
        let id = device.device_id.clone();
        store.create_device(&device, 1).unwrap();
        id
    }

    fn operation(
        device_id: &str,
        operation_id: &str,
        document_id: &str,
        logical_path: &str,
        title: &str,
        expected_revision: Option<u64>,
    ) -> PreparedDeviceDocumentMutation {
        let note = prepare_note(
            Some(document_id.into()),
            logical_path.into(),
            format!("#+title: {title}\n"),
            expected_revision,
        )
        .unwrap();
        prepare_device_document_mutation(1, operation_id.into(), device_id.into(), note).unwrap()
    }

    fn blob(operation: &PreparedDeviceDocumentMutation) -> BlobMeta {
        BlobMeta {
            sha256: format!("{:x}", Sha256::digest(operation.prepared.org.as_bytes())),
            size_bytes: operation.prepared.org.len() as u64,
            media_type: "text/org; charset=utf-8".into(),
        }
    }

    fn assert_applied_document(
        result: &DeviceMutationResult,
        document_id: &str,
        revision: u64,
        changed: bool,
    ) {
        let DeviceMutationResult::Applied {
            changed: actual_changed,
            authoritative: DeviceMutationProjection::Document(document),
            ..
        } = result
        else {
            panic!("expected applied document result");
        };
        assert_eq!(document.document_id, document_id);
        assert_eq!(document.revision, revision);
        assert_eq!(*actual_changed, changed);
    }

    fn assert_rejected_revision(
        result: &DeviceMutationResult,
        expected: Option<u64>,
        actual: Option<u64>,
        cursor: u64,
    ) {
        let DeviceMutationResult::Rejected {
            rejection:
                DeviceMutationRejection::RevisionConflict {
                    expected: stored_expected,
                    actual: stored_actual,
                },
            cursor: stored_cursor,
            ..
        } = result
        else {
            panic!("expected revision conflict");
        };
        assert_eq!((*stored_expected, *stored_actual), (expected, actual));
        assert_eq!(stored_cursor.position, cursor);
    }

    fn row_count(store: &SqliteStore, table: &str) -> u64 {
        store
            .connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
    }
}
