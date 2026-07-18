use rusqlite::{Connection, OptionalExtension, Transaction, params};
use sha2::{Digest as _, Sha256};

use super::{
    DeviceOperationInsert, SqliteStore, StoreError, advance_device_sequence,
    current_content_cursor, insert_blob, insert_content_change, insert_device_operation,
    replayed_device_operation, replica_cursor_token, require_device_sequence, scoped_operation_id,
    server_id, u64_to_i64, validate_replica_cursor,
};
use crate::application::{
    AssetReconciliationStatus, AuthoritativeAsset, OfflineAssetConflictReason,
    OfflineAssetOperation, OfflineAssetResult, path_key,
};
use crate::blob::BlobMeta;

impl SqliteStore {
    pub(crate) fn asset_operation_needs_blob(
        &mut self,
        format_version: u32,
        operation: &OfflineAssetOperation,
        canonical_payload: &str,
        actor_device_id: Option<&str>,
    ) -> Result<bool, StoreError> {
        require_asset_operation(format_version, operation, actor_device_id)?;
        let payload_digest = format!("{:x}", Sha256::digest(canonical_payload.as_bytes()));
        let transaction = self.write_transaction()?;
        if replayed_device_operation(
            &transaction,
            &operation.operation_id,
            "content",
            &payload_digest,
            &operation.device_id,
        )? {
            return Ok(false);
        }
        require_device_sequence(
            &transaction,
            &operation.device_id,
            operation.device_sequence,
        )?;
        staged_blob_conflict(&transaction, operation)?;
        Ok(true)
    }

    pub(crate) fn reconcile_asset_operation(
        &mut self,
        format_version: u32,
        operation: &OfflineAssetOperation,
        canonical_payload: &str,
        actor_device_id: Option<&str>,
        received_at_us: u64,
    ) -> Result<OfflineAssetResult, StoreError> {
        require_asset_operation(format_version, operation, actor_device_id)?;
        let payload_digest = format!("{:x}", Sha256::digest(canonical_payload.as_bytes()));
        let transaction = self.write_transaction()?;
        if replayed_device_operation(
            &transaction,
            &operation.operation_id,
            "content",
            &payload_digest,
            &operation.device_id,
        )? {
            return commit_replayed_result(
                transaction,
                &operation.device_id,
                &operation.operation_id,
            );
        }
        require_device_sequence(
            &transaction,
            &operation.device_id,
            operation.device_sequence,
        )?;
        let (blob_conflict, staged_size) = staged_blob_conflict(&transaction, operation)?;
        insert_asset_journal(
            &transaction,
            format_version,
            operation,
            canonical_payload,
            &payload_digest,
            received_at_us,
        )?;
        let conflict = asset_conflict_reason(&transaction, operation, blob_conflict)?;
        if conflict.is_some() {
            retain_conflict_evidence(&transaction, operation, staged_size, received_at_us)?;
        } else {
            promote_staged_blob(&transaction, operation, staged_size, received_at_us)?;
            apply_asset_put(&transaction, operation, received_at_us)?;
        }
        let result = asset_result(&transaction, format_version, operation, conflict)?;
        advance_device_sequence(
            &transaction,
            &operation.device_id,
            operation.device_sequence,
        )?;
        insert_asset_result(&transaction, &operation.device_id, &result, received_at_us)?;
        transaction.commit().map_err(StoreError::storage)?;
        Ok(result)
    }
}

fn require_asset_operation(
    format_version: u32,
    operation: &OfflineAssetOperation,
    actor_device_id: Option<&str>,
) -> Result<(), StoreError> {
    if format_version != 1 {
        return Err(StoreError::InvalidReviewState);
    }
    if actor_device_id.is_some_and(|actor| actor != operation.device_id) {
        return Err(StoreError::ReviewOwnership);
    }
    Ok(())
}

fn staged_blob_conflict(
    connection: &Connection,
    operation: &OfflineAssetOperation,
) -> Result<(Option<OfflineAssetConflictReason>, Option<u64>), StoreError> {
    let metadata = connection
        .query_row(
            "SELECT size_bytes, media_type FROM blob WHERE content_sha256 = ?1",
            [&operation.content_sha256],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(StoreError::storage)?;
    if let Some((size, media_type)) = metadata {
        let conflict = if u64::try_from(size).ok() != Some(operation.size_bytes)
            || media_type != operation.media_type
        {
            Some(OfflineAssetConflictReason::InvalidContent)
        } else {
            None
        };
        return Ok((conflict, None));
    }
    let staged_size = connection
        .query_row(
            "SELECT size_bytes FROM staged_blob WHERE content_sha256 = ?1",
            [&operation.content_sha256],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(StoreError::storage)?
        .and_then(|size| u64::try_from(size).ok())
        .ok_or_else(|| StoreError::BlobRequired {
            digest: operation.content_sha256.clone(),
        })?;
    let conflict =
        (staged_size != operation.size_bytes).then_some(OfflineAssetConflictReason::InvalidContent);
    Ok((conflict, Some(staged_size)))
}

fn promote_staged_blob(
    transaction: &Transaction<'_>,
    operation: &OfflineAssetOperation,
    staged_size: Option<u64>,
    received_at_us: u64,
) -> Result<(), StoreError> {
    if let Some(size_bytes) = staged_size {
        insert_blob(
            transaction,
            &BlobMeta {
                sha256: operation.content_sha256.clone(),
                size_bytes,
                media_type: operation.media_type.clone(),
            },
            u64_to_i64(received_at_us, "asset reconciliation timestamp")?,
        )?;
    }
    transaction
        .execute(
            "DELETE FROM staged_blob WHERE content_sha256 = ?1",
            [&operation.content_sha256],
        )
        .map(|_| ())
        .map_err(StoreError::storage)
}

fn retain_conflict_evidence(
    transaction: &Transaction<'_>,
    operation: &OfflineAssetOperation,
    staged_size: Option<u64>,
    received_at_us: u64,
) -> Result<(), StoreError> {
    if let Some(size_bytes) = staged_size {
        transaction
            .execute(
                "INSERT INTO offline_asset_evidence(
                     operation_id, content_sha256, size_bytes, media_type, retained_at_us
                 ) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    scoped_operation_id(&operation.device_id, &operation.operation_id),
                    operation.content_sha256,
                    u64_to_i64(size_bytes, "asset evidence size")?,
                    operation.media_type,
                    u64_to_i64(received_at_us, "asset evidence timestamp")?,
                ],
            )
            .map_err(StoreError::storage)?;
    }
    transaction
        .execute(
            "DELETE FROM staged_blob WHERE content_sha256 = ?1",
            [&operation.content_sha256],
        )
        .map(|_| ())
        .map_err(StoreError::storage)
}

fn insert_asset_journal(
    transaction: &Transaction<'_>,
    format_version: u32,
    operation: &OfflineAssetOperation,
    canonical_payload: &str,
    payload_digest: &str,
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
            payload_sha256: payload_digest,
            payload_json: canonical_payload,
            received_at_us,
        },
    )?;
    let operation_id = scoped_operation_id(&operation.device_id, &operation.operation_id);
    transaction
        .execute(
            "INSERT INTO offline_asset_operation(
                 operation_id, logical_path, original_name, content_sha256, size_bytes,
                 media_type, cursor_server_id, cursor, cursor_token, authored_at_us
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                operation_id,
                operation.logical_path,
                operation.original_name,
                operation.content_sha256,
                u64_to_i64(operation.size_bytes, "asset operation size")?,
                operation.media_type,
                operation.cursor_server_id,
                u64_to_i64(operation.cursor, "asset operation cursor")?,
                operation.cursor_token,
                u64_to_i64(operation.authored_at_us, "asset author timestamp")?,
            ],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn asset_conflict_reason(
    transaction: &Transaction<'_>,
    operation: &OfflineAssetOperation,
    blob_conflict: Option<OfflineAssetConflictReason>,
) -> Result<Option<OfflineAssetConflictReason>, StoreError> {
    match validate_replica_cursor(
        transaction,
        &operation.cursor_server_id,
        operation.cursor,
        &operation.cursor_token,
    ) {
        Ok(()) => {}
        Err(StoreError::ReplicaCursor | StoreError::ReplicaServer) => {
            return Ok(Some(OfflineAssetConflictReason::CursorChanged));
        }
        Err(error) => return Err(error),
    }
    if blob_conflict.is_some() {
        return Ok(blob_conflict);
    }
    if asset_exists(transaction, &operation.logical_path)? {
        return Ok(Some(OfflineAssetConflictReason::BaseChanged));
    }
    if asset_path_collides(transaction, &operation.logical_path)? {
        return Ok(Some(OfflineAssetConflictReason::PathConflict));
    }
    Ok(None)
}

fn asset_exists(connection: &Connection, logical_path: &str) -> Result<bool, StoreError> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM managed_asset WHERE asset_path = ?1)",
            [logical_path],
            |row| row.get(0),
        )
        .map_err(StoreError::storage)
}

fn asset_path_collides(connection: &Connection, logical_path: &str) -> Result<bool, StoreError> {
    connection
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM managed_asset WHERE path_key = ?1 AND asset_path <> ?2
             )",
            params![path_key(logical_path), logical_path],
            |row| row.get(0),
        )
        .map_err(StoreError::storage)
}

fn apply_asset_put(
    transaction: &Transaction<'_>,
    operation: &OfflineAssetOperation,
    received_at_us: u64,
) -> Result<(), StoreError> {
    let created_at_us = u64_to_i64(received_at_us, "asset reconciliation timestamp")?;
    transaction
        .execute(
            "INSERT INTO managed_asset(
                 asset_path, path_key, blob_sha256, original_name, revision,
                 active, created_at_us, updated_at_us
             ) VALUES (?1, ?2, ?3, ?4, 1, 1, ?5, ?5)",
            params![
                operation.logical_path,
                path_key(&operation.logical_path),
                operation.content_sha256,
                operation.original_name,
                created_at_us,
            ],
        )
        .map_err(StoreError::storage)?;
    transaction
        .execute(
            "INSERT INTO asset_revision(
                 asset_path, revision, blob_sha256, operation, created_at_us,
                 author_device_id
             ) VALUES (?1, 1, ?2, 'create', ?3, ?4)",
            params![
                operation.logical_path,
                operation.content_sha256,
                created_at_us,
                operation.device_id,
            ],
        )
        .map_err(StoreError::storage)?;
    insert_content_change(
        transaction,
        "asset",
        &operation.logical_path,
        1,
        "create",
        created_at_us,
    )
}

fn asset_result(
    transaction: &Transaction<'_>,
    format_version: u32,
    operation: &OfflineAssetOperation,
    conflict_reason: Option<OfflineAssetConflictReason>,
) -> Result<OfflineAssetResult, StoreError> {
    let cursor = current_content_cursor(transaction)?;
    Ok(OfflineAssetResult {
        operation_id: operation.operation_id.clone(),
        format_version,
        status: if conflict_reason.is_some() {
            AssetReconciliationStatus::Conflict
        } else {
            AssetReconciliationStatus::Accepted
        },
        conflict_reason,
        authoritative: current_asset_projection(transaction, &operation.logical_path)?,
        cursor,
        cursor_server_id: server_id(transaction)?,
        cursor_token: replica_cursor_token(transaction, cursor)?,
    })
}

fn current_asset_projection(
    connection: &Connection,
    logical_path: &str,
) -> Result<Option<AuthoritativeAsset>, StoreError> {
    connection
        .query_row(
            "SELECT asset.asset_path, asset.revision, asset.blob_sha256,
                    blob.size_bytes, blob.media_type, asset.active
             FROM managed_asset AS asset
             JOIN blob ON blob.content_sha256 = asset.blob_sha256
             WHERE asset.asset_path = ?1 AND asset.active = 1",
            [logical_path],
            |row| {
                Ok(AuthoritativeAsset {
                    logical_path: row.get(0)?,
                    revision: super::row_u64(row, 1)?,
                    content_sha256: row.get(2)?,
                    size_bytes: super::row_u64(row, 3)?,
                    media_type: row.get(4)?,
                    active: row.get(5)?,
                })
            },
        )
        .optional()
        .map_err(StoreError::storage)
}

fn insert_asset_result(
    transaction: &Transaction<'_>,
    device_id: &str,
    result: &OfflineAssetResult,
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
            "INSERT INTO offline_asset_result(
                 operation_id, format_version, status, conflict_reason,
                 authoritative_json, cursor, cursor_server_id, cursor_token,
                 reconciled_at_us
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                scoped_operation_id(device_id, &result.operation_id),
                i64::from(result.format_version),
                result.status.as_str(),
                result.conflict_reason.map(|reason| reason.as_str()),
                authoritative,
                u64_to_i64(result.cursor, "asset result cursor")?,
                result.cursor_server_id,
                result.cursor_token,
                u64_to_i64(reconciled_at_us, "asset reconciliation timestamp")?,
            ],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn commit_replayed_result(
    transaction: Transaction<'_>,
    device_id: &str,
    operation_id: &str,
) -> Result<OfflineAssetResult, StoreError> {
    let result = load_asset_result(&transaction, device_id, operation_id)?.ok_or(
        StoreError::CorruptData {
            field: "offline asset result",
        },
    )?;
    transaction.commit().map_err(StoreError::storage)?;
    Ok(result)
}

fn load_asset_result(
    connection: &Connection,
    device_id: &str,
    operation_id: &str,
) -> Result<Option<OfflineAssetResult>, StoreError> {
    let scoped_id = scoped_operation_id(device_id, operation_id);
    connection
        .query_row(
            "SELECT format_version, status, conflict_reason, authoritative_json,
                    cursor, cursor_server_id, cursor_token
             FROM offline_asset_result WHERE operation_id = ?1",
            [&scoped_id],
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
        .map(|row| materialize_asset_result(operation_id, row))
        .transpose()
}

type AssetResultRow = (
    i64,
    String,
    Option<String>,
    Option<String>,
    i64,
    String,
    String,
);

fn materialize_asset_result(
    operation_id: &str,
    row: AssetResultRow,
) -> Result<OfflineAssetResult, StoreError> {
    let status = match row.1.as_str() {
        "accepted" => AssetReconciliationStatus::Accepted,
        "conflict" => AssetReconciliationStatus::Conflict,
        _ => {
            return Err(StoreError::CorruptData {
                field: "asset result status",
            });
        }
    };
    Ok(OfflineAssetResult {
        operation_id: operation_id.to_owned(),
        format_version: u32::try_from(row.0).map_err(|_| StoreError::CorruptData {
            field: "asset result format",
        })?,
        status,
        conflict_reason: row.2.as_deref().map(parse_conflict_reason).transpose()?,
        authoritative: row
            .3
            .map(|json| serde_json::from_str(&json).map_err(StoreError::storage))
            .transpose()?,
        cursor: super::to_u64(row.4, "asset result cursor")?,
        cursor_server_id: row.5,
        cursor_token: row.6,
    })
}

fn parse_conflict_reason(value: &str) -> Result<OfflineAssetConflictReason, StoreError> {
    match value {
        "cursor_changed" => Ok(OfflineAssetConflictReason::CursorChanged),
        "base_changed" => Ok(OfflineAssetConflictReason::BaseChanged),
        "invalid_content" => Ok(OfflineAssetConflictReason::InvalidContent),
        "path_conflict" => Ok(OfflineAssetConflictReason::PathConflict),
        _ => Err(StoreError::CorruptData {
            field: "asset conflict reason",
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{SqliteStore, asset_exists};
    use crate::{
        application::{
            AssetReconciliationStatus, OfflineAssetConflictReason, OfflineAssetOperation,
            generate_device,
        },
        blob::BlobMeta,
        protocol::canonical_asset_operation,
    };

    fn operation(store: &SqliteStore, device_id: &str) -> OfflineAssetOperation {
        let state = store.replica_state(device_id).unwrap();
        OfflineAssetOperation {
            operation_id: "asset-operation".into(),
            device_id: device_id.into(),
            device_sequence: 1,
            cursor_server_id: state.server_id,
            cursor: state.cursor,
            cursor_token: state.cursor_token,
            authored_at_us: 10,
            logical_path: format!("assets/{}/image.png", "a".repeat(64)),
            original_name: "image.png".into(),
            content_sha256: "a".repeat(64),
            size_bytes: 4,
            media_type: "image/png".into(),
        }
    }

    #[test]
    fn missing_blob_does_not_consume_the_sequence_or_journal() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let device = generate_device("offline".into(), vec!["content:write".into()]).unwrap();
        let device_id = device.device_id.clone();
        store.create_device(&device, 1).unwrap();
        let operation = operation(&store, &device_id);
        let payload = canonical_asset_operation(&operation).unwrap();

        assert!(matches!(
            store.reconcile_asset_operation(1, &operation, &payload, Some(&device_id), 20),
            Err(crate::store::StoreError::BlobRequired { .. })
        ));
        assert_eq!(
            store
                .replica_state(&device_id)
                .unwrap()
                .next_device_sequence,
            1
        );
        assert_eq!(store.latest_content_cursor().unwrap(), 0);
    }

    #[test]
    fn operation_ids_are_scoped_to_device() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let devices = [
            generate_device("first".into(), vec!["content:write".into()]).unwrap(),
            generate_device("second".into(), vec!["content:write".into()]).unwrap(),
        ];
        for device in &devices {
            store.create_device(device, 1).unwrap();
        }
        let operations = [
            operation(&store, &devices[0].device_id),
            operation(&store, &devices[1].device_id),
        ];
        store
            .stage_blob(
                &BlobMeta {
                    sha256: operations[0].content_sha256.clone(),
                    size_bytes: operations[0].size_bytes,
                    media_type: operations[0].media_type.clone(),
                },
                5,
            )
            .unwrap();

        for (index, operation) in operations.iter().enumerate() {
            let payload = canonical_asset_operation(operation).unwrap();
            store
                .reconcile_asset_operation(
                    1,
                    operation,
                    &payload,
                    Some(&operation.device_id),
                    20 + u64::try_from(index).unwrap(),
                )
                .unwrap();
        }
        assert_eq!(
            store
                .connection
                .query_row("SELECT count(*) FROM offline_asset_operation", [], |row| {
                    row.get::<_, u64>(0)
                })
                .unwrap(),
            2
        );
    }

    #[test]
    fn accepts_exact_replay_and_preserves_active_collision() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let device = generate_device("offline".into(), vec!["content:write".into()]).unwrap();
        let device_id = device.device_id.clone();
        store.create_device(&device, 1).unwrap();
        let operation = operation(&store, &device_id);
        store
            .stage_blob(
                &BlobMeta {
                    sha256: operation.content_sha256.clone(),
                    size_bytes: operation.size_bytes,
                    media_type: operation.media_type.clone(),
                },
                5,
            )
            .unwrap();
        let payload = canonical_asset_operation(&operation).unwrap();

        let first = store
            .reconcile_asset_operation(1, &operation, &payload, Some(&device_id), 20)
            .unwrap();
        let replay = store
            .reconcile_asset_operation(1, &operation, &payload, Some(&device_id), 21)
            .unwrap();
        assert_eq!(first, replay);
        assert_eq!(first.status, AssetReconciliationStatus::Accepted);

        let state = store.replica_state(&device_id).unwrap();
        let mut collision = operation.clone();
        collision.operation_id = "second-operation".into();
        collision.device_sequence = 2;
        collision.cursor_server_id = state.server_id;
        collision.cursor = state.cursor;
        collision.cursor_token = state.cursor_token;
        let payload = canonical_asset_operation(&collision).unwrap();
        let result = store
            .reconcile_asset_operation(1, &collision, &payload, Some(&device_id), 22)
            .unwrap();
        assert_eq!(result.status, AssetReconciliationStatus::Conflict);
        assert_eq!(
            result.conflict_reason,
            Some(OfflineAssetConflictReason::BaseChanged)
        );
        assert_eq!(store.latest_content_cursor().unwrap(), 1);
    }

    #[test]
    fn staged_blob_metadata_mismatch_is_a_terminal_conflict() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let device = generate_device("offline".into(), vec!["content:write".into()]).unwrap();
        let device_id = device.device_id.clone();
        store.create_device(&device, 1).unwrap();
        let mut operation = operation(&store, &device_id);
        store
            .stage_blob(
                &BlobMeta {
                    sha256: operation.content_sha256.clone(),
                    size_bytes: operation.size_bytes,
                    media_type: operation.media_type.clone(),
                },
                5,
            )
            .unwrap();
        operation.size_bytes += 1;
        let payload = canonical_asset_operation(&operation).unwrap();

        let result = store
            .reconcile_asset_operation(1, &operation, &payload, Some(&device_id), 20)
            .unwrap();
        assert_eq!(result.status, AssetReconciliationStatus::Conflict);
        assert_eq!(
            result.conflict_reason,
            Some(OfflineAssetConflictReason::InvalidContent)
        );
        assert_eq!(
            store
                .replica_state(&device_id)
                .unwrap()
                .next_device_sequence,
            2
        );
        assert_eq!(store.latest_content_cursor().unwrap(), 0);
        assert!(!asset_exists(&store.connection, &operation.logical_path).unwrap());
        let canonical: bool = store
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM blob WHERE content_sha256 = ?1)",
                [&operation.content_sha256],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!canonical);
        let evidence: bool = store
            .connection
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM offline_asset_evidence WHERE operation_id = ?1
                 )",
                [super::scoped_operation_id(
                    &operation.device_id,
                    &operation.operation_id,
                )],
                |row| row.get(0),
            )
            .unwrap();
        assert!(evidence);
        assert_eq!(
            store.referenced_blobs().unwrap(),
            [(operation.content_sha256, operation.size_bytes - 1)]
        );
    }
}
