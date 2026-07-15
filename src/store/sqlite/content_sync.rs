use rusqlite::{Connection, OptionalExtension, Transaction, params};
use sha2::{Digest as _, Sha256};

use super::{
    DeviceOperationInsert, SqliteStore, StoreError, advance_device_sequence,
    current_content_cursor, insert_device_operation, load_document_head,
    load_document_replica_revision, rename_document_in, replace_note_in, replayed_device_operation,
    replica_cursor_token, require_device_sequence, server_id, tombstone_document_in, u64_to_i64,
    validate_replica_cursor,
};
use crate::{
    application::{
        AuthoritativeDocument, ContentBase, ContentReconciliationStatus,
        OfflineContentConflictReason, OfflineContentMutation, OfflineContentOperation,
        OfflineContentResult, PreparedNote,
    },
    blob::BlobMeta,
    domain::DocumentId,
};

impl SqliteStore {
    pub(crate) fn content_operation_needs_blob(
        &mut self,
        format_version: u32,
        operation: &OfflineContentOperation,
        canonical_payload: &str,
        actor_device_id: Option<&str>,
        prepared: Result<Option<&PreparedNote>, OfflineContentConflictReason>,
    ) -> Result<bool, StoreError> {
        if format_version != 1 {
            return Err(StoreError::InvalidReviewState);
        }
        require_content_actor(operation, actor_device_id)?;
        let digest = format!("{:x}", Sha256::digest(canonical_payload.as_bytes()));
        let transaction = self.write_transaction()?;
        if replayed_device_operation(
            &transaction,
            &operation.operation_id,
            "content",
            &digest,
            actor_device_id,
        )? {
            return Ok(false);
        }
        require_device_sequence(
            &transaction,
            &operation.device_id,
            operation.device_sequence,
        )?;
        Ok(operation.submitted_content_sha256().is_some()
            && content_conflict_reason(&transaction, operation, &prepared)?.is_none())
    }

    pub(crate) fn reconcile_content_operation(
        &mut self,
        format_version: u32,
        operation: &OfflineContentOperation,
        canonical_payload: &str,
        actor_device_id: Option<&str>,
        received_at_us: u64,
        prepared: Result<(Option<&PreparedNote>, Option<&BlobMeta>), OfflineContentConflictReason>,
    ) -> Result<OfflineContentResult, StoreError> {
        if format_version != 1 {
            return Err(StoreError::InvalidReviewState);
        }
        require_content_actor(operation, actor_device_id)?;
        let digest = format!("{:x}", Sha256::digest(canonical_payload.as_bytes()));
        let transaction = self.write_transaction()?;
        if replayed_device_operation(
            &transaction,
            &operation.operation_id,
            "content",
            &digest,
            actor_device_id,
        )? {
            return commit_replayed_result(transaction, &operation.operation_id);
        }
        require_device_sequence(
            &transaction,
            &operation.device_id,
            operation.device_sequence,
        )?;
        insert_content_journal(
            &transaction,
            format_version,
            operation,
            canonical_payload,
            &digest,
            received_at_us,
        )?;
        let result = reconcile_new_content(
            &transaction,
            format_version,
            operation,
            received_at_us,
            prepared,
        )?;
        advance_device_sequence(
            &transaction,
            &operation.device_id,
            operation.device_sequence,
        )?;
        insert_content_result(&transaction, &result, received_at_us)?;
        transaction.commit().map_err(StoreError::storage)?;
        Ok(result)
    }
}

fn require_content_actor(
    operation: &OfflineContentOperation,
    actor_device_id: Option<&str>,
) -> Result<(), StoreError> {
    if actor_device_id.is_none_or(|actor| actor == operation.device_id) {
        Ok(())
    } else {
        Err(StoreError::ReviewOwnership)
    }
}

fn commit_replayed_result(
    transaction: Transaction<'_>,
    operation_id: &str,
) -> Result<OfflineContentResult, StoreError> {
    let result =
        load_content_result(&transaction, operation_id)?.ok_or(StoreError::CorruptData {
            field: "offline content result",
        })?;
    transaction.commit().map_err(StoreError::storage)?;
    Ok(result)
}

fn insert_content_journal(
    transaction: &Transaction<'_>,
    format_version: u32,
    operation: &OfflineContentOperation,
    canonical_payload: &str,
    digest: &str,
    received_at_us: u64,
) -> Result<(), StoreError> {
    insert_device_operation(
        transaction,
        DeviceOperationInsert {
            operation_id: &operation.operation_id,
            device_id: &operation.device_id,
            device_sequence: operation.device_sequence,
            operation_kind: "content",
            format_version,
            payload_sha256: digest,
            payload_json: canonical_payload,
            received_at_us,
        },
    )?;
    insert_content_operation(transaction, operation)
}

fn insert_content_operation(
    transaction: &Transaction<'_>,
    operation: &OfflineContentOperation,
) -> Result<(), StoreError> {
    let (base_kind, base_revision, base_digest, predecessor) = match operation.base() {
        ContentBase::Absent => ("absent", None, None, None),
        ContentBase::Revision {
            revision,
            content_sha256,
            predecessor_operation_id,
        } => (
            "revision",
            Some(u64_to_i64(*revision, "document base revision")?),
            content_sha256.as_deref(),
            predecessor_operation_id.as_deref(),
        ),
    };
    transaction
        .execute(
            "INSERT INTO offline_content_operation(
                 operation_id, document_id, content_sha256, mutation, base_kind, base_revision,
                 base_content_sha256, predecessor_operation_id, cursor_server_id,
                 cursor, cursor_token, authored_at_us
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                operation.operation_id,
                operation.document_id(),
                operation.submitted_content_sha256(),
                operation.mutation_kind(),
                base_kind,
                base_revision,
                base_digest,
                predecessor,
                operation.cursor_server_id,
                u64_to_i64(operation.cursor, "content operation cursor")?,
                operation.cursor_token,
                u64_to_i64(operation.authored_at_us, "content author timestamp")?,
            ],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn reconcile_new_content(
    transaction: &Transaction<'_>,
    format_version: u32,
    operation: &OfflineContentOperation,
    received_at_us: u64,
    prepared: Result<(Option<&PreparedNote>, Option<&BlobMeta>), OfflineContentConflictReason>,
) -> Result<OfflineContentResult, StoreError> {
    let parsed = prepared
        .as_ref()
        .map(|(note, _)| *note)
        .map_err(|reason| *reason);
    let reason = content_conflict_reason(transaction, operation, &parsed)?;
    if let Some(reason) = reason {
        return content_result(transaction, format_version, operation, Some(reason));
    }
    let (prepared, blob) = prepared.map_err(|_| StoreError::CorruptData {
        field: "content preparation",
    })?;
    let expected = match operation.base() {
        ContentBase::Absent => None,
        ContentBase::Revision { revision, .. } => Some(*revision),
    };
    apply_content_mutation(
        transaction,
        operation,
        prepared,
        blob,
        expected,
        received_at_us,
    )?;
    content_result(transaction, format_version, operation, None)
}

fn apply_content_mutation(
    transaction: &Transaction<'_>,
    operation: &OfflineContentOperation,
    prepared: Option<&PreparedNote>,
    blob: Option<&BlobMeta>,
    expected_revision: Option<u64>,
    received_at_us: u64,
) -> Result<(), StoreError> {
    let timestamp = u64_to_i64(received_at_us, "content reconciliation timestamp")?;
    match &operation.mutation {
        OfflineContentMutation::Put { .. } => apply_document_put(
            transaction,
            operation,
            prepared,
            blob,
            expected_revision,
            timestamp,
        ),
        OfflineContentMutation::Rename { logical_path, .. } => rename_document_in(
            transaction,
            &document_id(operation)?,
            logical_path,
            expected_revision.ok_or(StoreError::InvalidReviewState)?,
            timestamp,
            Some(&operation.device_id),
        )
        .map(|_| ()),
        OfflineContentMutation::Tombstone { .. } => tombstone_document_in(
            transaction,
            &document_id(operation)?,
            expected_revision.ok_or(StoreError::InvalidReviewState)?,
            timestamp,
            Some(&operation.device_id),
        )
        .map(|_| ()),
    }
}

fn apply_document_put(
    transaction: &Transaction<'_>,
    operation: &OfflineContentOperation,
    prepared: Option<&PreparedNote>,
    blob: Option<&BlobMeta>,
    expected_revision: Option<u64>,
    timestamp: i64,
) -> Result<(), StoreError> {
    let prepared = prepared.ok_or(StoreError::CorruptData {
        field: "content preparation",
    })?;
    let blob = blob.ok_or(StoreError::CorruptData {
        field: "content blob preparation",
    })?;
    replace_note_in(
        transaction,
        prepared,
        blob,
        expected_revision,
        timestamp,
        Some(&operation.device_id),
        false,
    )
    .map(|_| ())
}

fn content_conflict_reason(
    transaction: &Transaction<'_>,
    operation: &OfflineContentOperation,
    prepared: &Result<Option<&PreparedNote>, OfflineContentConflictReason>,
) -> Result<Option<OfflineContentConflictReason>, StoreError> {
    match validate_replica_cursor(
        transaction,
        &operation.cursor_server_id,
        operation.cursor,
        &operation.cursor_token,
    ) {
        Ok(()) => {}
        Err(StoreError::ReplicaCursor | StoreError::ReplicaServer) => {
            return Ok(Some(OfflineContentConflictReason::CursorChanged));
        }
        Err(error) => return Err(error),
    }
    let document_id = document_id(operation)?;
    let head = load_document_head(transaction, &document_id)?;
    if !base_matches(transaction, operation, head.as_ref())? {
        return Ok(Some(OfflineContentConflictReason::BaseChanged));
    }
    if let Err(reason) = prepared {
        return Ok(Some(*reason));
    }
    let prepared = prepared.as_ref().map_err(|_| StoreError::CorruptData {
        field: "content preparation",
    })?;
    mutation_conflict_reason(transaction, operation, head.as_ref(), *prepared)
}

fn mutation_conflict_reason(
    transaction: &Transaction<'_>,
    operation: &OfflineContentOperation,
    head: Option<&crate::application::DocumentHead>,
    prepared: Option<&PreparedNote>,
) -> Result<Option<OfflineContentConflictReason>, StoreError> {
    let available = match &operation.mutation {
        OfflineContentMutation::Put { logical_path, .. } => {
            if head.is_some_and(|head| head.logical_path != *logical_path) {
                return Ok(Some(OfflineContentConflictReason::PathConflict));
            }
            let prepared = prepared.ok_or(StoreError::CorruptData {
                field: "content preparation",
            })?;
            match super::validate_note_assets(transaction, prepared) {
                Ok(()) => super::check_path_available(transaction, prepared),
                Err(StoreError::NotFound) => {
                    return Ok(Some(OfflineContentConflictReason::InvalidContent));
                }
                Err(error) => return Err(error),
            }
        }
        OfflineContentMutation::Rename { logical_path, .. } => {
            if head.is_none_or(|head| head.blob_sha256.is_none()) {
                return Ok(Some(OfflineContentConflictReason::InvalidContent));
            }
            super::check_document_path(transaction, &document_id(operation)?, logical_path)
        }
        OfflineContentMutation::Tombstone { .. } => return Ok(None),
    };
    match available {
        Ok(()) => Ok(None),
        Err(StoreError::InvalidProjection { .. }) => {
            Ok(Some(OfflineContentConflictReason::PathConflict))
        }
        Err(error) => Err(error),
    }
}

fn base_matches(
    connection: &Connection,
    operation: &OfflineContentOperation,
    head: Option<&crate::application::DocumentHead>,
) -> Result<bool, StoreError> {
    let ContentBase::Revision {
        revision,
        content_sha256,
        predecessor_operation_id,
    } = operation.base()
    else {
        return Ok(head.is_none());
    };
    let Some(head) = head.filter(|head| head.active && head.revision == *revision) else {
        return Ok(false);
    };
    let current_digest = current_document_digest(connection, head)?;
    let digest_matches = content_sha256
        .as_ref()
        .is_some_and(|expected| current_digest.as_ref() == Some(expected));
    Ok(digest_matches
        && predecessor_matches(connection, operation, predecessor_operation_id.as_deref())?)
}

fn current_document_digest(
    connection: &Connection,
    head: &crate::application::DocumentHead,
) -> Result<Option<String>, StoreError> {
    if head.blob_sha256.is_some() {
        return Ok(head.blob_sha256.clone());
    }
    let revision = load_document_replica_revision(connection, &head.document_id, head.revision)?;
    Ok(revision
        .projection
        .map(|projection| projection.content_sha256))
}

fn predecessor_matches(
    connection: &Connection,
    operation: &OfflineContentOperation,
    predecessor_id: Option<&str>,
) -> Result<bool, StoreError> {
    let Some(predecessor_id) = predecessor_id else {
        return base_was_observed_at_cursor(connection, operation);
    };
    let owner = connection
        .query_row(
            "SELECT device.device_id, content.document_id
             FROM device_operation AS device
             JOIN offline_content_operation AS content USING(operation_id)
             WHERE device.operation_id = ?1",
            [predecessor_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(StoreError::storage)?;
    if owner.as_ref()
        != Some(&(
            operation.device_id.clone(),
            operation.document_id().to_owned(),
        ))
    {
        return Ok(false);
    }
    let Some(result) = load_content_result(connection, predecessor_id)? else {
        return Ok(false);
    };
    Ok(result.status == ContentReconciliationStatus::Accepted
        && result
            .authoritative
            .as_ref()
            .is_some_and(|head| predecessor_authority_matches(operation.base(), head)))
}

fn base_was_observed_at_cursor(
    connection: &Connection,
    operation: &OfflineContentOperation,
) -> Result<bool, StoreError> {
    let ContentBase::Revision { revision, .. } = operation.base() else {
        return Ok(false);
    };
    connection
        .query_row(
            "SELECT NOT EXISTS(
                 SELECT 1 FROM content_change
                 WHERE entity_kind = 'document' AND entity_id = ?1
                   AND revision = ?2 AND cursor > ?3
             )",
            params![
                operation.document_id(),
                u64_to_i64(*revision, "document base revision")?,
                u64_to_i64(operation.cursor, "content operation cursor")?,
            ],
            |row| row.get(0),
        )
        .map_err(StoreError::storage)
}

fn predecessor_authority_matches(base: &ContentBase, head: &AuthoritativeDocument) -> bool {
    let ContentBase::Revision {
        revision,
        content_sha256,
        ..
    } = base
    else {
        return false;
    };
    head.active
        && head.revision == *revision
        && content_sha256
            .as_ref()
            .is_some_and(|digest| head.content_sha256.as_ref() == Some(digest))
}

fn document_id(operation: &OfflineContentOperation) -> Result<DocumentId, StoreError> {
    DocumentId::new(operation.document_id().to_owned()).map_err(|_| StoreError::InvalidProjection {
        reason: "invalid document ID",
    })
}

fn content_result(
    transaction: &Transaction<'_>,
    format_version: u32,
    operation: &OfflineContentOperation,
    conflict_reason: Option<OfflineContentConflictReason>,
) -> Result<OfflineContentResult, StoreError> {
    let authoritative = current_document_projection(transaction, operation)?;
    let cursor = current_content_cursor(transaction)?;
    Ok(OfflineContentResult {
        operation_id: operation.operation_id.clone(),
        format_version,
        status: if conflict_reason.is_some() {
            ContentReconciliationStatus::Conflict
        } else {
            ContentReconciliationStatus::Accepted
        },
        conflict_reason,
        authoritative,
        cursor,
        cursor_server_id: server_id(transaction)?,
        cursor_token: replica_cursor_token(transaction, cursor)?,
    })
}

fn current_document_projection(
    connection: &Connection,
    operation: &OfflineContentOperation,
) -> Result<Option<AuthoritativeDocument>, StoreError> {
    let id = document_id(operation)?;
    let Some(head) = load_document_head(connection, &id)? else {
        return Ok(None);
    };
    let content_sha256 = current_document_digest(connection, &head)?;
    Ok(Some(AuthoritativeDocument {
        document_id: head.document_id,
        logical_path: head.logical_path,
        revision: head.revision,
        content_sha256,
        active: head.active,
    }))
}

fn insert_content_result(
    transaction: &Transaction<'_>,
    result: &OfflineContentResult,
    reconciled_at_us: u64,
) -> Result<(), StoreError> {
    let authoritative = result
        .authoritative
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(StoreError::storage)?;
    transaction
        .execute(
            "INSERT INTO offline_content_result(
                 operation_id, format_version, status, conflict_reason,
                 authoritative_json, cursor, cursor_server_id, cursor_token,
                 reconciled_at_us
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                result.operation_id,
                i64::from(result.format_version),
                result.status.as_str(),
                result.conflict_reason.map(|reason| reason.as_str()),
                authoritative,
                u64_to_i64(result.cursor, "content result cursor")?,
                result.cursor_server_id,
                result.cursor_token,
                u64_to_i64(reconciled_at_us, "content reconciliation timestamp")?,
            ],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn load_content_result(
    connection: &Connection,
    operation_id: &str,
) -> Result<Option<OfflineContentResult>, StoreError> {
    connection
        .query_row(
            "SELECT format_version, status, conflict_reason, authoritative_json,
                    cursor, cursor_server_id, cursor_token
             FROM offline_content_result WHERE operation_id = ?1",
            [operation_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                ))
            },
        )
        .optional()
        .map_err(StoreError::storage)?
        .map(|row| materialize_content_result(operation_id, row))
        .transpose()
}

type ContentResultRow = (
    i64,
    String,
    Option<String>,
    Option<String>,
    i64,
    String,
    String,
);

fn materialize_content_result(
    operation_id: &str,
    row: ContentResultRow,
) -> Result<OfflineContentResult, StoreError> {
    let status = match row.1.as_str() {
        "accepted" => ContentReconciliationStatus::Accepted,
        "conflict" => ContentReconciliationStatus::Conflict,
        _ => {
            return Err(StoreError::CorruptData {
                field: "content result status",
            });
        }
    };
    Ok(OfflineContentResult {
        operation_id: operation_id.to_owned(),
        format_version: u32::try_from(row.0).map_err(|_| StoreError::CorruptData {
            field: "content result format",
        })?,
        status,
        conflict_reason: row.2.as_deref().map(parse_conflict_reason).transpose()?,
        authoritative: row
            .3
            .map(|json| serde_json::from_str(&json).map_err(StoreError::storage))
            .transpose()?,
        cursor: super::to_u64(row.4, "content result cursor")?,
        cursor_server_id: row.5,
        cursor_token: row.6,
    })
}

fn parse_conflict_reason(value: &str) -> Result<OfflineContentConflictReason, StoreError> {
    match value {
        "cursor_changed" => Ok(OfflineContentConflictReason::CursorChanged),
        "base_changed" => Ok(OfflineContentConflictReason::BaseChanged),
        "invalid_content" => Ok(OfflineContentConflictReason::InvalidContent),
        "path_conflict" => Ok(OfflineContentConflictReason::PathConflict),
        _ => Err(StoreError::CorruptData {
            field: "content conflict reason",
        }),
    }
}

#[cfg(test)]
mod tests {
    use sha2::{Digest as _, Sha256};

    use super::SqliteStore;
    use crate::{
        application::{
            ContentBase, ContentReconciliationStatus, OfflineContentConflictReason,
            OfflineContentMutation, OfflineContentOperation, generate_device, prepare_note,
            prepare_offline_document,
        },
        blob::BlobMeta,
        domain::{DocumentId, DocumentProjection, DocumentProjectionInput},
        protocol::canonical_content_operation,
    };

    fn add_device(store: &mut SqliteStore) -> String {
        let device = generate_device("offline".into(), vec!["content:write".into()]).unwrap();
        let id = device.device_id.clone();
        store.create_device(&device, 1).unwrap();
        id
    }

    fn operation(
        store: &SqliteStore,
        device_id: &str,
        operation_id: &str,
        sequence: u64,
        base: ContentBase,
        org: &str,
    ) -> OfflineContentOperation {
        let state = store.replica_state(device_id).unwrap();
        OfflineContentOperation {
            operation_id: operation_id.into(),
            device_id: device_id.into(),
            device_sequence: sequence,
            cursor_server_id: state.server_id,
            cursor: state.cursor,
            cursor_token: state.cursor_token,
            authored_at_us: sequence + 10,
            mutation: OfflineContentMutation::Put {
                document_id: "document".into(),
                logical_path: "note.org".into(),
                content_sha256: format!("{:x}", Sha256::digest(org.as_bytes())),
                org: org.into(),
                base,
            },
        }
    }

    fn blob(operation: &OfflineContentOperation) -> BlobMeta {
        let OfflineContentMutation::Put {
            content_sha256,
            org,
            ..
        } = &operation.mutation
        else {
            panic!("test operation must contain Org content");
        };
        BlobMeta {
            sha256: content_sha256.clone(),
            size_bytes: org.len() as u64,
            media_type: "text/org; charset=utf-8".into(),
        }
    }

    fn rename_operation(
        store: &SqliteStore,
        device_id: &str,
        operation_id: &str,
        sequence: u64,
        logical_path: &str,
        base: ContentBase,
    ) -> OfflineContentOperation {
        let state = store.replica_state(device_id).unwrap();
        OfflineContentOperation {
            operation_id: operation_id.into(),
            device_id: device_id.into(),
            device_sequence: sequence,
            cursor_server_id: state.server_id,
            cursor: state.cursor,
            cursor_token: state.cursor_token,
            authored_at_us: sequence + 10,
            mutation: OfflineContentMutation::Rename {
                document_id: "document".into(),
                logical_path: logical_path.into(),
                base,
            },
        }
    }

    fn tombstone_operation(
        store: &SqliteStore,
        device_id: &str,
        operation_id: &str,
        sequence: u64,
        base: ContentBase,
    ) -> OfflineContentOperation {
        let state = store.replica_state(device_id).unwrap();
        OfflineContentOperation {
            operation_id: operation_id.into(),
            device_id: device_id.into(),
            device_sequence: sequence,
            cursor_server_id: state.server_id,
            cursor: state.cursor,
            cursor_token: state.cursor_token,
            authored_at_us: sequence + 10,
            mutation: OfflineContentMutation::Tombstone {
                document_id: "document".into(),
                base,
            },
        }
    }

    #[test]
    fn accepts_and_exactly_replays_document_create() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let device_id = add_device(&mut store);
        let operation = operation(
            &store,
            &device_id,
            "operation-1",
            1,
            ContentBase::Absent,
            "#+title: Note\n* Heading\n",
        );
        let prepared = prepare_offline_document(&operation).unwrap();
        let blob = blob(&operation);
        let result = store
            .reconcile_content_operation(
                1,
                &operation,
                "canonical-operation-1",
                Some(&device_id),
                100,
                Ok((prepared.as_ref(), Some(&blob))),
            )
            .unwrap();
        assert_eq!(result.status, ContentReconciliationStatus::Accepted);
        assert_eq!(result.authoritative.as_ref().unwrap().revision, 1);
        assert_eq!(
            store
                .replica_state(&device_id)
                .unwrap()
                .next_device_sequence,
            2
        );

        let replay = store
            .reconcile_content_operation(
                1,
                &operation,
                "canonical-operation-1",
                Some(&device_id),
                101,
                Ok((prepared.as_ref(), Some(&blob))),
            )
            .unwrap();
        assert_eq!(replay, result);
        assert!(matches!(
            store.reconcile_content_operation(
                1,
                &operation,
                "changed-operation",
                Some(&device_id),
                102,
                Ok((prepared.as_ref(), Some(&blob))),
            ),
            Err(crate::store::StoreError::IdempotencyConflict)
        ));
    }

    #[test]
    fn rename_then_tombstone_preserves_history_and_blocks_resurrection() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let device_id = add_device(&mut store);
        let create = operation(
            &store,
            &device_id,
            "create",
            1,
            ContentBase::Absent,
            "#+title: Original\n* Heading\n",
        );
        let prepared = prepare_offline_document(&create).unwrap();
        let create_result = store
            .reconcile_content_operation(
                1,
                &create,
                "create-payload",
                Some(&device_id),
                100,
                Ok((prepared.as_ref(), Some(&blob(&create)))),
            )
            .unwrap();
        let digest = create_result
            .authoritative
            .as_ref()
            .unwrap()
            .content_sha256
            .clone()
            .unwrap();
        let rename = rename_operation(
            &store,
            &device_id,
            "rename",
            2,
            "renamed.org",
            ContentBase::Revision {
                revision: 1,
                content_sha256: Some(digest.clone()),
                predecessor_operation_id: Some("create".into()),
            },
        );
        let renamed = store
            .reconcile_content_operation(
                1,
                &rename,
                "rename-payload",
                Some(&device_id),
                101,
                Ok((None, None)),
            )
            .unwrap();
        assert_eq!(renamed.status, ContentReconciliationStatus::Accepted);
        assert_eq!(renamed.authoritative.as_ref().unwrap().revision, 2);
        assert_eq!(
            renamed.authoritative.as_ref().unwrap().content_sha256,
            Some(digest.clone())
        );

        let tombstone = tombstone_operation(
            &store,
            &device_id,
            "tombstone",
            3,
            ContentBase::Revision {
                revision: 2,
                content_sha256: Some(digest.clone()),
                predecessor_operation_id: Some("rename".into()),
            },
        );
        let deleted = store
            .reconcile_content_operation(
                1,
                &tombstone,
                "tombstone-payload",
                Some(&device_id),
                102,
                Ok((None, None)),
            )
            .unwrap();
        assert_eq!(deleted.status, ContentReconciliationStatus::Accepted);
        assert!(!deleted.authoritative.as_ref().unwrap().active);
        assert!(
            deleted
                .authoritative
                .as_ref()
                .unwrap()
                .content_sha256
                .is_none()
        );

        let resurrection = operation(
            &store,
            &device_id,
            "resurrection",
            4,
            ContentBase::Revision {
                revision: 2,
                content_sha256: Some(digest),
                predecessor_operation_id: Some("rename".into()),
            },
            "#+title: Resurrected\n",
        );
        let prepared = prepare_offline_document(&resurrection).unwrap();
        let rejected = store
            .reconcile_content_operation(
                1,
                &resurrection,
                "resurrection-payload",
                Some(&device_id),
                103,
                Ok((prepared.as_ref(), Some(&blob(&resurrection)))),
            )
            .unwrap();
        assert_eq!(
            rejected.conflict_reason,
            Some(OfflineContentConflictReason::BaseChanged)
        );
        let id = DocumentId::new("document").unwrap();
        let revisions = store.document_revisions(&id, 0, 10).unwrap();
        assert_eq!(
            revisions
                .iter()
                .map(|revision| revision.operation.as_str())
                .collect::<Vec<_>>(),
            ["create", "rename", "tombstone"]
        );
        assert!(!store.document_head(&id).unwrap().unwrap().active);
    }

    #[test]
    fn rename_path_collision_is_a_durable_conflict() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        for (document_id, logical_path) in [("document", "note.org"), ("other", "taken.org")] {
            let note = prepare_note(
                Some(document_id.into()),
                logical_path.into(),
                format!("#+title: {document_id}\n"),
                None,
            )
            .unwrap();
            let blob = BlobMeta {
                sha256: note.blob_sha256.clone(),
                size_bytes: note.org.len() as u64,
                media_type: "text/org; charset=utf-8".into(),
            };
            store.replace_note(&note, &blob, 1, None).unwrap();
        }
        let device_id = add_device(&mut store);
        let head = store
            .document_head(&DocumentId::new("document").unwrap())
            .unwrap()
            .unwrap();
        let rename = rename_operation(
            &store,
            &device_id,
            "rename-conflict",
            1,
            "taken.org",
            ContentBase::Revision {
                revision: head.revision,
                content_sha256: head.blob_sha256,
                predecessor_operation_id: None,
            },
        );
        let result = store
            .reconcile_content_operation(
                1,
                &rename,
                "rename-conflict-payload",
                Some(&device_id),
                2,
                Ok((None, None)),
            )
            .unwrap();
        assert_eq!(
            result.conflict_reason,
            Some(OfflineContentConflictReason::PathConflict)
        );
        assert_eq!(result.authoritative.unwrap().logical_path, "note.org");
        assert_eq!(
            store
                .replica_state(&device_id)
                .unwrap()
                .next_device_sequence,
            2
        );
    }

    #[test]
    fn projection_only_rename_is_preserved_as_an_invalid_content_conflict() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let projection = DocumentProjection::new(DocumentProjectionInput {
            title: "Projection".into(),
            file_node_id: None,
            file_tags: vec![],
            nodes: vec![],
            link_occurrences: vec![],
            compatibility_sha1: "a".repeat(40),
            content_sha256: "b".repeat(64),
        })
        .unwrap();
        let id = DocumentId::new("document").unwrap();
        store.replace_document(&id, &projection, None).unwrap();
        super::super::backfill_current_replica_history(&store.connection).unwrap();
        let device_id = add_device(&mut store);
        let rename = rename_operation(
            &store,
            &device_id,
            "projection-rename",
            1,
            "renamed.org",
            ContentBase::Revision {
                revision: 1,
                content_sha256: Some("b".repeat(64)),
                predecessor_operation_id: None,
            },
        );
        let result = store
            .reconcile_content_operation(
                1,
                &rename,
                "projection-rename-payload",
                Some(&device_id),
                2,
                Ok((None, None)),
            )
            .unwrap();
        assert_eq!(
            result.conflict_reason,
            Some(OfflineContentConflictReason::InvalidContent)
        );
        assert_eq!(
            result.authoritative.unwrap().content_sha256,
            Some("b".repeat(64))
        );
        assert_eq!(store.document_head(&id).unwrap().unwrap().revision, 1);
        assert_eq!(
            store
                .replica_state(&device_id)
                .unwrap()
                .next_device_sequence,
            2
        );
    }

    #[test]
    fn invalid_org_is_preserved_without_replacing_the_head() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let original = prepare_note(
            Some("document".into()),
            "note.org".into(),
            ":PROPERTIES:\n:ID: root-id\n:END:\n#+title: Original\n\nSee [[id:target-id][Target]].\n\n* Target\n:PROPERTIES:\n:ID: target-id\n:END:\n".into(),
            None,
        )
        .unwrap();
        let original_blob = BlobMeta {
            sha256: original.blob_sha256.clone(),
            size_bytes: original.org.len() as u64,
            media_type: "text/org; charset=utf-8".into(),
        };
        store
            .replace_note(&original, &original_blob, 1, None)
            .unwrap();
        let device_id = add_device(&mut store);
        let id = DocumentId::new("document").unwrap();
        let head = store.document_head(&id).unwrap().unwrap();
        let operation = operation(
            &store,
            &device_id,
            "operation-invalid",
            1,
            ContentBase::Revision {
                revision: head.revision,
                content_sha256: head.blob_sha256.clone(),
                predecessor_operation_id: None,
            },
            "* [[id:broken\n",
        );
        assert!(prepare_offline_document(&operation).is_err());
        let canonical_payload = canonical_content_operation(&operation).unwrap();
        let root = store.get_node("root-id").unwrap();
        let target = store.get_node("target-id").unwrap();
        let links = store.graph_nodes("root-id", false, 10, None).unwrap();
        let history = store.document_revisions(&id, 0, 10).unwrap();
        let changes = store.content_changes(0, 10).unwrap();
        let cursor = store.latest_content_cursor().unwrap();
        let result = store
            .reconcile_content_operation(
                1,
                &operation,
                &canonical_payload,
                Some(&device_id),
                100,
                Err(OfflineContentConflictReason::InvalidContent),
            )
            .unwrap();
        assert_eq!(result.status, ContentReconciliationStatus::Conflict);
        assert_eq!(
            result.conflict_reason,
            Some(OfflineContentConflictReason::InvalidContent)
        );
        assert_eq!(
            result.authoritative.as_ref().unwrap().revision,
            head.revision
        );
        assert_eq!(store.document_head(&id).unwrap(), Some(head));
        assert_eq!(store.get_node("root-id").unwrap(), root);
        assert_eq!(store.get_node("target-id").unwrap(), target);
        assert_eq!(
            store.graph_nodes("root-id", false, 10, None).unwrap(),
            links
        );
        assert_eq!(store.document_revisions(&id, 0, 10).unwrap(), history);
        assert_eq!(store.content_changes(0, 10).unwrap(), changes);
        assert_eq!(store.latest_content_cursor().unwrap(), cursor);
        assert_eq!(
            super::load_content_result(&store.connection, "operation-invalid").unwrap(),
            Some(result)
        );
        let stored_payload = store
            .connection
            .query_row(
                "SELECT payload_json FROM device_operation WHERE operation_id = ?1",
                ["operation-invalid"],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        assert_eq!(stored_payload, canonical_payload);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&stored_payload).unwrap()["operation"]["mutation"]
                ["org"],
            "* [[id:broken\n"
        );
        assert_eq!(
            store
                .replica_state(&device_id)
                .unwrap()
                .next_device_sequence,
            2
        );
    }

    #[test]
    fn missing_asset_is_preserved_without_replacing_the_head() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let device_id = add_device(&mut store);
        let missing = format!("assets/{}/missing.png", "a".repeat(64));
        let operation = operation(
            &store,
            &device_id,
            "operation-missing-asset",
            1,
            ContentBase::Absent,
            &format!("#+title: Offline\n[[file:{missing}]]\n"),
        );
        let prepared = prepare_offline_document(&operation).unwrap();
        let before = store.latest_content_cursor().unwrap();

        let result = store
            .reconcile_content_operation(
                1,
                &operation,
                "missing-asset-operation",
                Some(&device_id),
                100,
                Ok((prepared.as_ref(), Some(&blob(&operation)))),
            )
            .unwrap();

        assert_eq!(result.status, ContentReconciliationStatus::Conflict);
        assert_eq!(
            result.conflict_reason,
            Some(OfflineContentConflictReason::InvalidContent)
        );
        assert!(result.authoritative.is_none());
        assert_eq!(store.latest_content_cursor().unwrap(), before);
        assert_eq!(
            store
                .replica_state(&device_id)
                .unwrap()
                .next_device_sequence,
            2
        );
    }

    #[test]
    fn expired_cursor_is_a_terminal_conflict_without_a_canonical_write() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let device_id = add_device(&mut store);
        let operation = operation(
            &store,
            &device_id,
            "expired-cursor",
            1,
            ContentBase::Absent,
            "#+title: Offline\n* Heading\n",
        );
        let prepared = prepare_offline_document(&operation).unwrap();
        let blob = blob(&operation);
        let online = prepare_note(
            Some("other-document".into()),
            "other.org".into(),
            "#+title: Other\n* Heading\n".into(),
            None,
        )
        .unwrap();
        let online_blob = BlobMeta {
            sha256: online.blob_sha256.clone(),
            size_bytes: online.org.len() as u64,
            media_type: "text/org; charset=utf-8".into(),
        };
        store.replace_note(&online, &online_blob, 50, None).unwrap();
        let before = store.latest_content_cursor().unwrap();
        store.expire_replica_changes_through(before).unwrap();

        let result = store
            .reconcile_content_operation(
                1,
                &operation,
                "expired-cursor-operation",
                Some(&device_id),
                100,
                Ok((prepared.as_ref(), Some(&blob))),
            )
            .unwrap();
        assert_eq!(
            result.conflict_reason,
            Some(OfflineContentConflictReason::CursorChanged)
        );
        assert!(result.authoritative.is_none());
        assert_eq!(store.latest_content_cursor().unwrap(), before);
    }

    #[test]
    fn online_update_wins_and_stale_offline_revision_is_retained() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let device_id = add_device(&mut store);
        let create = operation(
            &store,
            &device_id,
            "create",
            1,
            ContentBase::Absent,
            "#+title: Original\n* Heading\n",
        );
        let create_prepared = prepare_offline_document(&create).unwrap();
        let create_blob = blob(&create);
        store
            .reconcile_content_operation(
                1,
                &create,
                "create-operation",
                Some(&device_id),
                100,
                Ok((create_prepared.as_ref(), Some(&create_blob))),
            )
            .unwrap();

        let stale = operation(
            &store,
            &device_id,
            "stale",
            2,
            ContentBase::Revision {
                revision: 1,
                content_sha256: Some(create.submitted_content_sha256().unwrap().to_owned()),
                predecessor_operation_id: Some(create.operation_id.clone()),
            },
            "#+title: Offline\n* Heading\n",
        );
        let stale_prepared = prepare_offline_document(&stale).unwrap();
        let stale_blob = blob(&stale);
        let online = prepare_note(
            Some("document".into()),
            "note.org".into(),
            "#+title: Online\n* Heading\n".into(),
            Some(1),
        )
        .unwrap();
        let online_blob = BlobMeta {
            sha256: online.blob_sha256.clone(),
            size_bytes: online.org.len() as u64,
            media_type: "text/org; charset=utf-8".into(),
        };
        store
            .replace_note(&online, &online_blob, 150, Some(&device_id))
            .unwrap();
        let before = store.latest_content_cursor().unwrap();

        let result = store
            .reconcile_content_operation(
                1,
                &stale,
                "stale-operation",
                Some(&device_id),
                200,
                Ok((stale_prepared.as_ref(), Some(&stale_blob))),
            )
            .unwrap();
        assert_eq!(
            result.conflict_reason,
            Some(OfflineContentConflictReason::BaseChanged)
        );
        assert_eq!(result.authoritative.unwrap().revision, 2);
        assert_eq!(store.latest_content_cursor().unwrap(), before);
    }

    #[test]
    fn conflicted_predecessor_cannot_authorize_a_matching_predicted_revision() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let device_id = add_device(&mut store);
        let original = prepare_note(
            Some("document".into()),
            "note.org".into(),
            "#+title: Original\n".into(),
            None,
        )
        .unwrap();
        store
            .replace_note(
                &original,
                &BlobMeta {
                    sha256: original.blob_sha256.clone(),
                    size_bytes: original.org.len() as u64,
                    media_type: "text/org; charset=utf-8".into(),
                },
                1,
                None,
            )
            .unwrap();
        let first = operation(
            &store,
            &device_id,
            "first",
            1,
            ContentBase::Revision {
                revision: 1,
                content_sha256: Some(original.blob_sha256.clone()),
                predecessor_operation_id: None,
            },
            "#+title: Predicted\n",
        );
        let second = operation(
            &store,
            &device_id,
            "second",
            2,
            ContentBase::Revision {
                revision: 2,
                content_sha256: Some(first.submitted_content_sha256().unwrap().to_owned()),
                predecessor_operation_id: Some(first.operation_id.clone()),
            },
            "#+title: Second\n",
        );
        let forged = operation(
            &store,
            &device_id,
            "forged",
            3,
            ContentBase::Revision {
                revision: 2,
                content_sha256: Some(first.submitted_content_sha256().unwrap().to_owned()),
                predecessor_operation_id: None,
            },
            "#+title: Forged\n",
        );
        let online = prepare_note(
            Some("document".into()),
            "note.org".into(),
            "#+title: Predicted\n".into(),
            Some(1),
        )
        .unwrap();
        store.replace_note(&online, &blob(&first), 2, None).unwrap();

        for operation in [&first, &second, &forged] {
            let prepared = prepare_offline_document(operation).unwrap();
            let result = store
                .reconcile_content_operation(
                    1,
                    operation,
                    &format!("payload-{}", operation.operation_id),
                    Some(&device_id),
                    3 + operation.device_sequence,
                    Ok((prepared.as_ref(), Some(&blob(operation)))),
                )
                .unwrap();
            assert_eq!(
                result.conflict_reason,
                Some(OfflineContentConflictReason::BaseChanged)
            );
        }
        assert_eq!(
            store
                .document_head(&DocumentId::new("document").unwrap())
                .unwrap()
                .unwrap()
                .revision,
            2
        );
    }
}
