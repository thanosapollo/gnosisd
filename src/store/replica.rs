use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt, path::Path, time::Duration};

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::application::{
    AssetReconciliationStatus, ContentBase, ContentReconciliationStatus, OfflineAnswerError,
    OfflineAnswerInput, OfflineAssetConflictReason, OfflineAssetOperation,
    OfflineContentConflictReason, OfflineContentMutation, OfflineContentOperation,
    OfflineReviewOperation, OfflineReviewResult, ReconciliationStatus, ReviewPackView,
    ReviewSessionMode, path_key, prepare_offline_operation, validate_org_path,
};

const CURRENT_SCHEMA: &str = include_str!("schema/replica_v1.sql");
const APPLICATION_ID: u32 = 0x474e4f52;
const SCHEMA_VERSION: u32 = 1;
const FIRST_REQUEST: &str = "";

pub(crate) struct LocalReplicaStore {
    connection: Connection,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LocalReplicaItem {
    pub(crate) entity_kind: String,
    pub(crate) entity_id: String,
    pub(crate) revision: u64,
    pub(crate) tombstone: bool,
    pub(crate) blob_digests: Vec<String>,
    pub(crate) payload_json: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SnapshotInstall {
    pub(crate) snapshot_id: String,
    pub(crate) server_id: String,
    pub(crate) cut_cursor: u64,
    pub(crate) cut_token: String,
    pub(crate) requested: Option<String>,
    pub(crate) after: u64,
    pub(crate) next: Option<String>,
    pub(crate) next_after: Option<u64>,
    pub(crate) cursor: Option<String>,
    pub(crate) response_sha256: String,
    pub(crate) items: Vec<LocalReplicaItem>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DeltaInstall {
    pub(crate) server_id: String,
    pub(crate) from: String,
    pub(crate) next: String,
    pub(crate) items: Vec<LocalReplicaItem>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LocalReplicaStatus {
    pub(crate) server_id: Option<String>,
    pub(crate) observed_server_id: Option<String>,
    pub(crate) cursor: Option<String>,
    pub(crate) snapshot_next: Option<String>,
    pub(crate) next_device_sequence: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PendingReviewOperation {
    pub(crate) operation: OfflineReviewOperation,
}

pub(crate) struct LocalContentContext {
    pub(crate) server_id: String,
    pub(crate) cursor: String,
    pub(crate) next_device_sequence: u64,
    pub(crate) base: ContentBase,
}

pub(crate) struct PendingContentOperation {
    pub(crate) canonical_payload: String,
}

pub(crate) struct LocalAssetContext {
    pub(crate) server_id: String,
    pub(crate) cursor: String,
    pub(crate) next_device_sequence: u64,
    pub(crate) max_blob_bytes: u64,
}

pub(crate) struct PendingAssetOperation {
    pub(crate) canonical_payload: String,
    pub(crate) bytes: Vec<u8>,
}

pub(crate) struct LocalAssetOverlay {
    pub(crate) content_sha256: String,
    pub(crate) size_bytes: u64,
    pub(crate) media_type: String,
    pub(crate) bytes: Vec<u8>,
}

pub(crate) struct AssetResultInstall {
    pub(crate) operation_id: String,
    pub(crate) status: AssetReconciliationStatus,
    pub(crate) conflict_reason: Option<OfflineAssetConflictReason>,
    pub(crate) cursor_server_id: String,
    pub(crate) authoritative_path: Option<String>,
}

pub(crate) struct ContentResultInstall {
    pub(crate) operation_id: String,
    pub(crate) status: ContentReconciliationStatus,
    pub(crate) conflict_reason: Option<OfflineContentConflictReason>,
    pub(crate) cursor_server_id: String,
    pub(crate) authoritative_document_id: Option<String>,
}

pub(crate) struct StoredContentResult {
    pub(crate) sequence: u64,
    pub(crate) server_id: String,
    pub(crate) operation_json: String,
    pub(crate) result_json: String,
}

pub(crate) struct StoredContentResultPage {
    pub(crate) items: Vec<StoredContentResult>,
    pub(crate) next_after: Option<u64>,
}

pub(crate) struct StoredAssetResult {
    pub(crate) sequence: u64,
    pub(crate) server_id: String,
    pub(crate) operation_json: String,
    pub(crate) result_json: String,
}

pub(crate) struct StoredAssetResultPage {
    pub(crate) items: Vec<StoredAssetResult>,
    pub(crate) next_after: Option<u64>,
}

pub(crate) struct StoredReviewPack {
    pub(crate) pack: ReviewPackView,
    pub(crate) local_position: u32,
    pub(crate) status: String,
}

pub(crate) struct StoredReviewResult {
    pub(crate) sequence: u64,
    pub(crate) server_id: String,
    pub(crate) operation: OfflineReviewOperation,
    pub(crate) result_json: String,
}

pub(crate) struct StoredReviewResultPage {
    pub(crate) items: Vec<StoredReviewResult>,
    pub(crate) next_after: Option<u64>,
}

#[derive(Debug, Error)]
pub(crate) enum LocalReplicaError {
    #[error("local replica storage failed")]
    Storage(#[source] rusqlite::Error),
    #[error("local replica filesystem operation failed")]
    Io(#[source] std::io::Error),
    #[error("local replica data is corrupt")]
    Corrupt,
    #[error("local replica schema is unsupported")]
    Schema,
    #[error("local replica belongs to another device")]
    Device,
    #[error("server identity changed while the outbox is pending")]
    ServerChanged,
    #[error("replica synchronization page does not follow durable state")]
    SyncOrder,
    #[error("replica projection revision is stale or inconsistent")]
    Projection,
    #[error("replica snapshot page changed across a retry")]
    SnapshotChanged,
    #[error("review pack assets are incomplete")]
    MissingAssets,
    #[error("offline answer no longer matches the current pack position")]
    StaleAnswer,
    #[error("offline answer is invalid")]
    Answer(#[from] OfflineAnswerError),
    #[error("offline operation result is out of order or inconsistent")]
    Result,
    #[error("offline content operation no longer matches the replica state")]
    Content,
    #[error("offline content path is already owned by another document")]
    PathConflict,
}

impl LocalReplicaStore {
    pub(crate) fn open(path: impl AsRef<Path>, device_id: &str) -> Result<Self, LocalReplicaError> {
        let path = path.as_ref();
        let mut connection = Connection::open(path).map_err(LocalReplicaError::Storage)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(LocalReplicaError::Io)?;
        connection
            .pragma_update(None, "foreign_keys", true)
            .map_err(LocalReplicaError::Storage)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(LocalReplicaError::Storage)?;
        initialize(&mut connection, device_id)?;
        Ok(Self { connection })
    }

    pub(crate) fn open_existing(path: impl AsRef<Path>) -> Result<Self, LocalReplicaError> {
        let path = path.as_ref();
        let mut connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
            .map_err(LocalReplicaError::Storage)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(LocalReplicaError::Io)?;
        connection
            .pragma_update(None, "foreign_keys", true)
            .map_err(LocalReplicaError::Storage)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(LocalReplicaError::Storage)?;
        let device_id = existing_device_id(&connection)?;
        initialize(&mut connection, &device_id)?;
        Ok(Self { connection })
    }

    pub(crate) fn observe_server(
        &mut self,
        server_id: &str,
        next_device_sequence: u64,
    ) -> Result<(), LocalReplicaError> {
        let transaction = self.immediate()?;
        let (active, observed) = replica_servers(&transaction)?;
        let changed = active.as_deref().is_some_and(|active| active != server_id)
            || observed
                .as_deref()
                .is_some_and(|observed| observed != server_id);
        if changed && has_pending_outbox(&transaction)? {
            return Err(LocalReplicaError::ServerChanged);
        }
        transaction
            .execute(
                "UPDATE replica_identity SET observed_server_id = ?1,
                     offline_document_lifecycle = CASE WHEN ?2 = 1 THEN 0
                         ELSE offline_document_lifecycle END,
                     offline_asset_put = CASE WHEN ?2 = 1 THEN 0 ELSE offline_asset_put END,
                     offline_asset_max_bytes = CASE WHEN ?2 = 1 THEN 0
                         ELSE offline_asset_max_bytes END
                 WHERE id = 1",
                params![server_id, i64::from(changed)],
            )
            .map_err(LocalReplicaError::Storage)?;
        let sequence = u64_to_i64(next_device_sequence)?;
        if changed {
            transaction
                .execute(
                    "UPDATE replica_sync SET next_device_sequence = ?1 WHERE id = 1",
                    [sequence],
                )
                .map_err(LocalReplicaError::Storage)?;
        } else {
            transaction
                .execute(
                    "UPDATE replica_sync SET next_device_sequence = max(next_device_sequence, ?1)
                     WHERE id = 1",
                    [sequence],
                )
                .map_err(LocalReplicaError::Storage)?;
        }
        transaction.commit().map_err(LocalReplicaError::Storage)
    }

    pub(crate) fn install_content_capabilities(
        &mut self,
        server_id: &str,
    ) -> Result<(), LocalReplicaError> {
        let transaction = self.immediate()?;
        let observed = transaction
            .query_row(
                "SELECT observed_server_id FROM replica_identity WHERE id = 1",
                [],
                |row| row.get::<_, Option<String>>(0),
            )
            .map_err(LocalReplicaError::Storage)?;
        if observed.as_deref() != Some(server_id) {
            return Err(LocalReplicaError::SyncOrder);
        }
        transaction
            .execute(
                "UPDATE replica_identity SET offline_document_lifecycle = 1 WHERE id = 1",
                [],
            )
            .map_err(LocalReplicaError::Storage)?;
        transaction.commit().map_err(LocalReplicaError::Storage)
    }

    pub(crate) fn install_asset_capabilities(
        &mut self,
        server_id: &str,
        max_blob_bytes: u64,
    ) -> Result<(), LocalReplicaError> {
        let transaction = self.immediate()?;
        let observed = transaction
            .query_row(
                "SELECT observed_server_id FROM replica_identity WHERE id = 1",
                [],
                |row| row.get::<_, Option<String>>(0),
            )
            .map_err(LocalReplicaError::Storage)?;
        if observed.as_deref() != Some(server_id) {
            return Err(LocalReplicaError::SyncOrder);
        }
        transaction
            .execute(
                "UPDATE replica_identity SET offline_asset_put = 1,
                     offline_asset_max_bytes = ?1 WHERE id = 1",
                [u64_to_i64(max_blob_bytes)?],
            )
            .map_err(LocalReplicaError::Storage)?;
        transaction.commit().map_err(LocalReplicaError::Storage)
    }

    pub(crate) fn status(&self) -> Result<LocalReplicaStatus, LocalReplicaError> {
        self.connection
            .query_row(
                "SELECT identity.server_id, identity.observed_server_id, sync.cursor,
                        sync.snapshot_next, sync.next_device_sequence
                 FROM replica_identity AS identity
                 JOIN replica_sync AS sync ON sync.id = identity.id WHERE identity.id = 1",
                [],
                |row| {
                    Ok(LocalReplicaStatus {
                        server_id: row.get(0)?,
                        observed_server_id: row.get(1)?,
                        cursor: row.get(2)?,
                        snapshot_next: row.get(3)?,
                        next_device_sequence: row_u64(row, 4)?,
                    })
                },
            )
            .map_err(LocalReplicaError::Storage)
    }

    pub(crate) fn device_id(&self) -> Result<String, LocalReplicaError> {
        self.connection
            .query_row(
                "SELECT device_id FROM replica_identity WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .map_err(LocalReplicaError::Storage)
    }

    pub(crate) fn install_snapshot(
        &mut self,
        page: SnapshotInstall,
    ) -> Result<(), LocalReplicaError> {
        let transaction = self.immediate()?;
        if replayed_snapshot_page(&transaction, &page)? {
            transaction.commit().map_err(LocalReplicaError::Storage)?;
            return Ok(());
        }
        prepare_snapshot(&transaction, &page)?;
        validate_snapshot_continuation(&transaction, &page)?;
        insert_snapshot_items(&transaction, &page)?;
        record_snapshot_page(&transaction, &page)?;
        if page.cursor.is_some() {
            promote_snapshot(&transaction, &page)?;
        } else {
            advance_snapshot(&transaction, &page)?;
        }
        transaction.commit().map_err(LocalReplicaError::Storage)
    }

    pub(crate) fn install_delta(&mut self, page: DeltaInstall) -> Result<(), LocalReplicaError> {
        let transaction = self.immediate()?;
        let status = sync_state(&transaction)?;
        if status.snapshot_id.is_some()
            || status.server_id.as_deref() != Some(page.server_id.as_str())
            || status.cursor.as_deref() != Some(page.from.as_str())
        {
            return Err(LocalReplicaError::SyncOrder);
        }
        for item in &page.items {
            install_projection(&transaction, item)?;
        }
        transaction
            .execute(
                "UPDATE replica_sync SET cursor = ?1 WHERE id = 1",
                [&page.next],
            )
            .map_err(LocalReplicaError::Storage)?;
        transaction.commit().map_err(LocalReplicaError::Storage)
    }

    pub(crate) fn install_blob(
        &mut self,
        digest: &str,
        bytes: &[u8],
    ) -> Result<(), LocalReplicaError> {
        if format!("{:x}", Sha256::digest(bytes)) != digest {
            return Err(LocalReplicaError::Corrupt);
        }
        let existing = self
            .connection
            .query_row(
                "SELECT bytes FROM replica_blob WHERE content_sha256 = ?1",
                [digest],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(LocalReplicaError::Storage)?;
        if let Some(existing) = existing {
            return if existing == bytes {
                Ok(())
            } else {
                Err(LocalReplicaError::Corrupt)
            };
        }
        self.connection
            .execute(
                "INSERT INTO replica_blob(content_sha256, size_bytes, bytes) VALUES (?1, ?2, ?3)",
                params![
                    digest,
                    u64_to_i64(
                        u64::try_from(bytes.len()).map_err(|_| LocalReplicaError::Corrupt)?
                    )?,
                    bytes
                ],
            )
            .map_err(LocalReplicaError::Storage)?;
        Ok(())
    }

    pub(crate) fn missing_projection_blobs(&self) -> Result<Vec<String>, LocalReplicaError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT DISTINCT projection.content_sha256
                 FROM replica_projection_blob AS projection
                 LEFT JOIN replica_blob AS blob USING(content_sha256)
                 WHERE blob.content_sha256 IS NULL ORDER BY projection.content_sha256",
            )
            .map_err(LocalReplicaError::Storage)?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(LocalReplicaError::Storage)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(LocalReplicaError::Storage)
    }

    pub(crate) fn missing_blobs(&self) -> Result<Vec<String>, LocalReplicaError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT required.content_sha256 FROM (
                     SELECT content_sha256 FROM replica_projection_blob
                     UNION
                     SELECT content_sha256 FROM replica_review_pack_asset
                 ) AS required
                 LEFT JOIN replica_blob AS blob USING(content_sha256)
                 WHERE blob.content_sha256 IS NULL ORDER BY required.content_sha256",
            )
            .map_err(LocalReplicaError::Storage)?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(LocalReplicaError::Storage)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(LocalReplicaError::Storage)
    }

    pub(crate) fn blob(&self, digest: &str) -> Result<Option<Vec<u8>>, LocalReplicaError> {
        let bytes = self
            .connection
            .query_row(
                "SELECT bytes FROM replica_blob WHERE content_sha256 = ?1",
                [digest],
                |row| row.get(0),
            )
            .optional()
            .map_err(LocalReplicaError::Storage)?;
        match bytes {
            Some(bytes) if format!("{:x}", Sha256::digest(&bytes)) == digest => Ok(Some(bytes)),
            Some(_) => Err(LocalReplicaError::Corrupt),
            None => Ok(None),
        }
    }

    pub(crate) fn review_pack(
        &self,
        pack_id: &str,
    ) -> Result<Option<StoredReviewPack>, LocalReplicaError> {
        let stored = self
            .connection
            .query_row(
                "SELECT pack.pack_json, progress.local_position, progress.status
                 FROM replica_review_pack AS pack
                 JOIN replica_review_progress AS progress USING(pack_id)
                 WHERE pack.pack_id = ?1",
                [pack_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(LocalReplicaError::Storage)?;
        stored.map(decode_stored_pack).transpose()
    }

    pub(crate) fn active_review_pack(&self) -> Result<Option<StoredReviewPack>, LocalReplicaError> {
        let pack_id = self
            .connection
            .query_row(
                "SELECT pack.pack_id FROM replica_review_pack AS pack
                 JOIN replica_review_progress AS progress USING(pack_id)
                 JOIN replica_identity AS identity ON identity.id = 1
                 WHERE progress.status = 'active'
                   AND pack.server_id = identity.server_id
                   AND pack.server_id = identity.observed_server_id",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(LocalReplicaError::Storage)?;
        pack_id
            .map(|pack_id| self.review_pack(&pack_id))
            .transpose()
            .map(Option::flatten)
    }

    pub(crate) fn install_review_pack(
        &mut self,
        pack: &ReviewPackView,
        cursor: &str,
    ) -> Result<(), LocalReplicaError> {
        let transaction = self.immediate()?;
        validate_pack_owner(&transaction, pack)?;
        let json = serde_json::to_string(pack).map_err(|_| LocalReplicaError::Corrupt)?;
        if let Some(existing) = stored_pack_json(&transaction, &pack.pack_id)? {
            if existing == json {
                transaction.commit().map_err(LocalReplicaError::Storage)?;
                return Ok(());
            }
            if apply_existing_pack_update(&transaction, &existing, pack)? {
                transaction.commit().map_err(LocalReplicaError::Storage)?;
                return Ok(());
            }
            return Err(LocalReplicaError::Projection);
        }
        if pack.status == "active" {
            transaction
                .execute(
                    "UPDATE replica_review_progress SET status = 'abandoned'
                     WHERE status = 'active'",
                    [],
                )
                .map_err(LocalReplicaError::Storage)?;
        }
        insert_review_pack(&transaction, pack, cursor, &json)?;
        transaction.commit().map_err(LocalReplicaError::Storage)
    }

    pub(crate) fn record_answer(
        &mut self,
        pack_id: &str,
        input: OfflineAnswerInput,
    ) -> Result<OfflineReviewOperation, LocalReplicaError> {
        let transaction = self.immediate()?;
        if let Some(operation) = replayed_answer(&transaction, pack_id, &input)? {
            transaction.commit().map_err(LocalReplicaError::Storage)?;
            return Ok(operation);
        }
        require_current_pack_server(&transaction, pack_id)?;
        let (pack, position) = load_pack_for_answer(&transaction, pack_id)?;
        if position != input.session_position {
            return Err(LocalReplicaError::StaleAnswer);
        }
        require_pack_assets(&transaction, pack_id)?;
        let sequence = next_device_sequence(&transaction)?;
        let operation = prepare_offline_operation(&pack, sequence, input)?;
        insert_review_operation(&transaction, &operation)?;
        advance_review_progress(&transaction, &pack, position)?;
        transaction
            .execute(
                "UPDATE replica_sync SET next_device_sequence = next_device_sequence + 1
                 WHERE id = 1",
                [],
            )
            .map_err(LocalReplicaError::Storage)?;
        transaction.commit().map_err(LocalReplicaError::Storage)?;
        Ok(operation)
    }

    pub(crate) fn next_pending_operation(
        &self,
    ) -> Result<Option<PendingReviewOperation>, LocalReplicaError> {
        self.connection
            .query_row(
                "SELECT operation.operation_json
                 FROM replica_device_operation AS device
                 JOIN replica_review_operation AS operation
                   ON operation.attempt_id = device.operation_id
                 LEFT JOIN replica_review_result AS result USING(attempt_id)
                 WHERE result.attempt_id IS NULL
                   AND NOT EXISTS(
                       SELECT 1 FROM replica_device_operation AS earlier
                       LEFT JOIN replica_review_result AS review
                           ON earlier.operation_kind = 'review'
                          AND review.attempt_id = earlier.operation_id
                       LEFT JOIN replica_content_result AS content
                           ON earlier.operation_kind = 'content'
                          AND content.operation_id = earlier.operation_id
                       LEFT JOIN replica_asset_result AS asset
                           ON asset.operation_id = earlier.operation_id
                       WHERE earlier.server_id = device.server_id
                         AND earlier.device_sequence < device.device_sequence
                         AND review.attempt_id IS NULL
                         AND content.operation_id IS NULL
                         AND asset.operation_id IS NULL
                   )
                 ORDER BY device.device_sequence LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(LocalReplicaError::Storage)?
            .map(|json| {
                serde_json::from_str(&json)
                    .map(|operation| PendingReviewOperation { operation })
                    .map_err(|_| LocalReplicaError::Corrupt)
            })
            .transpose()
    }

    pub(crate) fn content_context(
        &self,
        document_id: &str,
    ) -> Result<LocalContentContext, LocalReplicaError> {
        let context = self
            .connection
            .query_row(
                "SELECT identity.server_id, identity.observed_server_id, sync.cursor,
                        sync.snapshot_id, sync.next_device_sequence
                 FROM replica_identity AS identity
                 JOIN replica_sync AS sync ON sync.id = identity.id WHERE identity.id = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row_u64(row, 4)?,
                    ))
                },
            )
            .map_err(LocalReplicaError::Storage)?;
        let server_id = context.0.ok_or(LocalReplicaError::Content)?;
        if context.1.as_deref() != Some(server_id.as_str()) || context.3.is_some() {
            return Err(LocalReplicaError::Content);
        }
        let cursor = context.2.ok_or(LocalReplicaError::Content)?;
        let base = working_content_base(&self.connection, &server_id, document_id)?;
        if matches!(
            &base,
            ContentBase::Revision {
                content_sha256: None,
                ..
            }
        ) {
            return Err(LocalReplicaError::Content);
        }
        Ok(LocalContentContext {
            server_id,
            cursor,
            next_device_sequence: context.4,
            base,
        })
    }

    pub(crate) fn existing_content_operation(
        &self,
        operation_id: &str,
    ) -> Result<Option<String>, LocalReplicaError> {
        let existing = self
            .connection
            .query_row(
                "SELECT device.operation_kind, content.operation_json
                 FROM replica_device_operation AS device
                 LEFT JOIN replica_content_operation AS content USING(operation_id)
                 WHERE device.operation_id = ?1",
                [operation_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()
            .map_err(LocalReplicaError::Storage)?;
        match existing {
            None => Ok(None),
            Some((kind, Some(json))) if kind == "content" => Ok(Some(json)),
            Some(_) => Err(LocalReplicaError::Content),
        }
    }

    pub(crate) fn require_document_assets(
        &self,
        asset_paths: &[String],
    ) -> Result<(), LocalReplicaError> {
        let server_id = self
            .current_server_id()?
            .ok_or(LocalReplicaError::MissingAssets)?;
        for path in asset_paths {
            let projection = self
                .connection
                .query_row(
                    "SELECT projection.tombstone, EXISTS(
                         SELECT 1 FROM replica_projection_blob AS required
                         JOIN replica_blob AS blob USING(content_sha256)
                         WHERE required.entity_kind = projection.entity_kind
                           AND required.entity_id = projection.entity_id
                     )
                     FROM replica_projection AS projection
                     WHERE projection.entity_kind = 'asset' AND projection.entity_id = ?1",
                    [path],
                    |row| Ok((row.get::<_, bool>(0)?, row.get::<_, bool>(1)?)),
                )
                .optional()
                .map_err(LocalReplicaError::Storage)?;
            let available = match projection {
                Some((tombstone, has_blob)) => !tombstone && has_blob,
                None => pending_or_accepted_asset(&self.connection, &server_id, path)?,
            };
            if !available {
                return Err(LocalReplicaError::MissingAssets);
            }
        }
        Ok(())
    }

    pub(crate) fn record_content_operation(
        &mut self,
        operation: &OfflineContentOperation,
        cursor: &str,
        canonical_payload: &str,
    ) -> Result<(), LocalReplicaError> {
        let transaction = self.immediate()?;
        if replayed_content_operation(&transaction, operation, canonical_payload)? {
            transaction.commit().map_err(LocalReplicaError::Storage)?;
            return Ok(());
        }
        require_content_context(&transaction, operation, cursor)?;
        insert_local_content_operation(&transaction, operation, canonical_payload)?;
        transaction
            .execute(
                "UPDATE replica_sync SET next_device_sequence = next_device_sequence + 1
                 WHERE id = 1",
                [],
            )
            .map_err(LocalReplicaError::Storage)?;
        transaction.commit().map_err(LocalReplicaError::Storage)
    }

    pub(crate) fn next_pending_content_operation(
        &self,
    ) -> Result<Option<PendingContentOperation>, LocalReplicaError> {
        let pending = self
            .connection
            .query_row(
                "SELECT content.operation_json
                 FROM replica_device_operation AS device
                 JOIN replica_content_operation AS content USING(operation_id)
                 LEFT JOIN replica_content_result AS result USING(operation_id)
                 WHERE result.operation_id IS NULL
                   AND NOT EXISTS(
                       SELECT 1 FROM replica_device_operation AS earlier
                       LEFT JOIN replica_review_result AS review
                           ON earlier.operation_kind = 'review'
                          AND review.attempt_id = earlier.operation_id
                       LEFT JOIN replica_content_result AS prior_content
                           ON earlier.operation_kind = 'content'
                          AND prior_content.operation_id = earlier.operation_id
                       LEFT JOIN replica_asset_result AS asset
                           ON asset.operation_id = earlier.operation_id
                       WHERE earlier.server_id = device.server_id
                         AND earlier.device_sequence < device.device_sequence
                         AND review.attempt_id IS NULL
                         AND prior_content.operation_id IS NULL
                         AND asset.operation_id IS NULL
                   )
                 ORDER BY device.device_sequence LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(LocalReplicaError::Storage)?
            .map(|canonical_payload| PendingContentOperation { canonical_payload });
        Ok(pending)
    }

    pub(crate) fn record_content_result(
        &mut self,
        result: &ContentResultInstall,
        cursor: &str,
        result_json: &str,
    ) -> Result<(), LocalReplicaError> {
        let transaction = self.immediate()?;
        if replayed_content_result(&transaction, &result.operation_id, result_json)? {
            transaction.commit().map_err(LocalReplicaError::Storage)?;
            return Ok(());
        }
        require_next_content_result(&transaction, result)?;
        transaction
            .execute(
                "INSERT INTO replica_content_result(
                     operation_id, status, conflict_reason, result_json
                 ) VALUES (?1, ?2, ?3, ?4)",
                params![
                    result.operation_id,
                    result.status.as_str(),
                    result.conflict_reason.map(|reason| reason.as_str()),
                    result_json,
                ],
            )
            .map_err(LocalReplicaError::Storage)?;
        transaction
            .execute(
                "UPDATE replica_sync SET result_cursor_hint = ?1 WHERE id = 1",
                [cursor],
            )
            .map_err(LocalReplicaError::Storage)?;
        transaction.commit().map_err(LocalReplicaError::Storage)
    }

    pub(crate) fn asset_context(
        &self,
        logical_path: &str,
    ) -> Result<LocalAssetContext, LocalReplicaError> {
        let context = self
            .connection
            .query_row(
                "SELECT identity.server_id, identity.observed_server_id, sync.cursor,
                        sync.snapshot_id, sync.next_device_sequence, identity.offline_asset_put,
                        identity.offline_asset_max_bytes
                 FROM replica_identity AS identity
                 JOIN replica_sync AS sync ON sync.id = identity.id WHERE identity.id = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row_u64(row, 4)?,
                        row.get::<_, bool>(5)?,
                        row_u64(row, 6)?,
                    ))
                },
            )
            .map_err(LocalReplicaError::Storage)?;
        let server_id = context.0.ok_or(LocalReplicaError::Content)?;
        if context.1.as_deref() != Some(server_id.as_str())
            || context.2.is_none()
            || context.3.is_some()
            || !context.5
            || local_asset_exists(&self.connection, &server_id, logical_path)?
        {
            return Err(LocalReplicaError::Content);
        }
        Ok(LocalAssetContext {
            server_id,
            cursor: context.2.ok_or(LocalReplicaError::Content)?,
            next_device_sequence: context.4,
            max_blob_bytes: context.6,
        })
    }

    pub(crate) fn existing_asset_operation(
        &self,
        operation_id: &str,
    ) -> Result<Option<String>, LocalReplicaError> {
        let existing = self
            .connection
            .query_row(
                "SELECT device.operation_kind, asset.operation_json
                 FROM replica_device_operation AS device
                 LEFT JOIN replica_asset_operation AS asset USING(operation_id)
                 WHERE device.operation_id = ?1",
                [operation_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()
            .map_err(LocalReplicaError::Storage)?;
        match existing {
            None => Ok(None),
            Some((kind, Some(json))) if kind == "content" => Ok(Some(json)),
            Some(_) => Err(LocalReplicaError::Content),
        }
    }

    pub(crate) fn asset_overlay(
        &self,
        logical_path: &str,
    ) -> Result<Option<LocalAssetOverlay>, LocalReplicaError> {
        let Some(server_id) = self.current_server_id()? else {
            return Ok(None);
        };
        self.connection
            .query_row(
                "SELECT operation.content_sha256, operation.size_bytes,
                        operation.media_type, blob.bytes
                 FROM replica_asset_operation AS operation
                 JOIN replica_device_operation AS device USING(operation_id)
                 JOIN replica_blob AS blob USING(content_sha256)
                 LEFT JOIN replica_asset_result AS result USING(operation_id)
                 WHERE operation.logical_path = ?1 AND device.server_id = ?2
                   AND (result.operation_id IS NULL OR result.status = 'accepted')
                 ORDER BY device.device_sequence DESC LIMIT 1",
                params![logical_path, server_id],
                |row| {
                    Ok(LocalAssetOverlay {
                        content_sha256: row.get(0)?,
                        size_bytes: row_u64(row, 1)?,
                        media_type: row.get(2)?,
                        bytes: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(LocalReplicaError::Storage)
    }

    pub(crate) fn record_asset_operation(
        &mut self,
        operation: &OfflineAssetOperation,
        cursor: &str,
        canonical_payload: &str,
        bytes: &[u8],
    ) -> Result<(), LocalReplicaError> {
        let transaction = self.immediate()?;
        if replayed_asset_operation(&transaction, operation, canonical_payload, bytes)? {
            transaction.commit().map_err(LocalReplicaError::Storage)?;
            return Ok(());
        }
        require_asset_context(&transaction, operation, cursor)?;
        install_blob_in(&transaction, &operation.content_sha256, bytes)?;
        insert_local_asset_operation(&transaction, operation, canonical_payload)?;
        transaction
            .execute(
                "UPDATE replica_sync SET next_device_sequence = next_device_sequence + 1
                 WHERE id = 1",
                [],
            )
            .map_err(LocalReplicaError::Storage)?;
        transaction.commit().map_err(LocalReplicaError::Storage)
    }

    pub(crate) fn next_pending_asset_operation(
        &self,
    ) -> Result<Option<PendingAssetOperation>, LocalReplicaError> {
        self.connection
            .query_row(
                "SELECT asset.operation_json, blob.bytes
                 FROM replica_device_operation AS device
                 JOIN replica_asset_operation AS asset USING(operation_id)
                 JOIN replica_blob AS blob USING(content_sha256)
                 LEFT JOIN replica_asset_result AS result USING(operation_id)
                 WHERE result.operation_id IS NULL
                   AND NOT EXISTS(
                       SELECT 1 FROM replica_device_operation AS earlier
                       LEFT JOIN replica_review_result AS review
                           ON earlier.operation_kind = 'review'
                          AND review.attempt_id = earlier.operation_id
                       LEFT JOIN replica_content_result AS content
                           ON content.operation_id = earlier.operation_id
                       LEFT JOIN replica_asset_result AS prior_asset
                           ON prior_asset.operation_id = earlier.operation_id
                       WHERE earlier.server_id = device.server_id
                         AND earlier.device_sequence < device.device_sequence
                         AND review.attempt_id IS NULL
                         AND content.operation_id IS NULL
                         AND prior_asset.operation_id IS NULL
                   )
                 ORDER BY device.device_sequence LIMIT 1",
                [],
                |row| {
                    Ok(PendingAssetOperation {
                        canonical_payload: row.get(0)?,
                        bytes: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(LocalReplicaError::Storage)
    }

    pub(crate) fn record_asset_result(
        &mut self,
        result: &AssetResultInstall,
        cursor: &str,
        result_json: &str,
    ) -> Result<(), LocalReplicaError> {
        let transaction = self.immediate()?;
        if replayed_asset_result(&transaction, &result.operation_id, result_json)? {
            transaction.commit().map_err(LocalReplicaError::Storage)?;
            return Ok(());
        }
        require_next_asset_result(&transaction, result)?;
        transaction
            .execute(
                "INSERT INTO replica_asset_result(
                     operation_id, status, conflict_reason, result_json
                 ) VALUES (?1, ?2, ?3, ?4)",
                params![
                    result.operation_id,
                    result.status.as_str(),
                    result.conflict_reason.map(|reason| reason.as_str()),
                    result_json,
                ],
            )
            .map_err(LocalReplicaError::Storage)?;
        transaction
            .execute(
                "UPDATE replica_sync SET result_cursor_hint = ?1 WHERE id = 1",
                [cursor],
            )
            .map_err(LocalReplicaError::Storage)?;
        transaction.commit().map_err(LocalReplicaError::Storage)
    }

    pub(crate) fn content_results(
        &self,
        after: u64,
        limit: u16,
        conflicts_only: bool,
    ) -> Result<StoredContentResultPage, LocalReplicaError> {
        if limit == 0 || limit > 1000 {
            return Err(LocalReplicaError::Result);
        }
        load_content_results(&self.connection, after, limit, conflicts_only)
    }

    pub(crate) fn asset_results(
        &self,
        after: u64,
        limit: u16,
        conflicts_only: bool,
    ) -> Result<StoredAssetResultPage, LocalReplicaError> {
        if limit == 0 || limit > 1000 {
            return Err(LocalReplicaError::Result);
        }
        load_asset_results(&self.connection, after, limit, conflicts_only)
    }

    pub(crate) fn record_result(
        &mut self,
        result: &OfflineReviewResult,
        cursor: &str,
        result_json: &str,
    ) -> Result<(), LocalReplicaError> {
        let transaction = self.immediate()?;
        if replayed_result(&transaction, &result.attempt_id, result_json)? {
            transaction.commit().map_err(LocalReplicaError::Storage)?;
            return Ok(());
        }
        require_next_result(&transaction, result)?;
        transaction
            .execute(
                "INSERT INTO replica_review_result(
                     attempt_id, status, conflict_reason, result_json
                 ) VALUES (?1, ?2, ?3, ?4)",
                params![
                    result.attempt_id,
                    result.status.as_str(),
                    result.conflict_reason.map(|reason| reason.as_str()),
                    result_json,
                ],
            )
            .map_err(LocalReplicaError::Storage)?;
        transaction
            .execute(
                "UPDATE replica_sync SET result_cursor_hint = ?1 WHERE id = 1",
                [cursor],
            )
            .map_err(LocalReplicaError::Storage)?;
        transaction.commit().map_err(LocalReplicaError::Storage)
    }

    pub(crate) fn review_results(
        &self,
        after: u64,
        limit: u16,
        conflicts_only: bool,
    ) -> Result<StoredReviewResultPage, LocalReplicaError> {
        if limit == 0 || limit > 1000 {
            return Err(LocalReplicaError::Result);
        }
        let mut statement = self
            .connection
            .prepare(
                "SELECT result.result_sequence, operation.server_id,
                        operation.operation_json, result.result_json
                 FROM replica_review_operation AS operation
                 JOIN replica_review_result AS result USING(attempt_id)
                 WHERE result.result_sequence > ?1
                   AND (?2 = 0 OR result.status = 'conflict')
                 ORDER BY result.result_sequence LIMIT ?3",
            )
            .map_err(LocalReplicaError::Storage)?;
        let rows = statement
            .query_map(
                params![
                    u64_to_i64(after)?,
                    i64::from(conflicts_only),
                    i64::from(limit) + 1,
                ],
                |row| {
                    Ok((
                        row_u64(row, 0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .map_err(LocalReplicaError::Storage)?;
        let mut items = rows
            .map(|row| -> Result<StoredReviewResult, LocalReplicaError> {
                let (sequence, server_id, operation, result_json) =
                    row.map_err(LocalReplicaError::Storage)?;
                Ok(StoredReviewResult {
                    sequence,
                    server_id,
                    operation: serde_json::from_str(&operation)
                        .map_err(|_| LocalReplicaError::Corrupt)?,
                    result_json,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = items.len() > usize::from(limit);
        if has_more {
            items.pop();
        }
        let next_after = has_more.then(|| items.last().expect("nonzero page limit").sequence);
        Ok(StoredReviewResultPage { items, next_after })
    }

    pub(crate) fn projection(
        &self,
        kind: &str,
        id: &str,
    ) -> Result<Option<LocalReplicaItem>, LocalReplicaError> {
        let stored = self
            .connection
            .query_row(
                "SELECT revision, tombstone, payload_json FROM replica_projection
                 WHERE entity_kind = ?1 AND entity_id = ?2",
                params![kind, id],
                |row| {
                    Ok((
                        row_u64(row, 0)?,
                        row.get::<_, i64>(1)? != 0,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(LocalReplicaError::Storage)?;
        stored
            .map(|(revision, tombstone, payload_json)| {
                Ok(LocalReplicaItem {
                    entity_kind: kind.into(),
                    entity_id: id.into(),
                    revision,
                    tombstone,
                    blob_digests: load_projection_blobs(&self.connection, kind, id)?,
                    payload_json,
                })
            })
            .transpose()
    }

    pub(crate) fn current_server_id(&self) -> Result<Option<String>, LocalReplicaError> {
        let (active, observed) = replica_servers(&self.connection)?;
        if active == observed {
            Ok(active)
        } else {
            Err(LocalReplicaError::SyncOrder)
        }
    }

    pub(crate) fn document_id_for_path(
        &self,
        logical_path: &str,
    ) -> Result<Option<String>, LocalReplicaError> {
        validate_org_path(logical_path).map_err(|_| LocalReplicaError::Content)?;
        let Some(server_id) = self.current_server_id()? else {
            return Ok(None);
        };
        let documents = working_documents(&self.connection, &server_id)?;
        let wanted = path_key(logical_path);
        let mut found = None;
        for (document_id, document) in documents {
            let Some(path) = document.logical_path else {
                continue;
            };
            if path_key(&path) != wanted {
                continue;
            }
            if found.replace(document_id).is_some() {
                return Err(LocalReplicaError::PathConflict);
            }
        }
        Ok(found)
    }

    #[cfg(test)]
    pub(crate) fn counts(&self) -> Result<(u64, u64, u64), LocalReplicaError> {
        let projections = count(&self.connection, "replica_projection")?;
        let blobs = count(&self.connection, "replica_blob")?;
        let results = count(&self.connection, "replica_review_result")?;
        Ok((projections, blobs, results))
    }

    #[cfg(test)]
    pub(crate) fn review_progress(&self, pack_id: &str) -> Result<u32, LocalReplicaError> {
        self.connection
            .query_row(
                "SELECT local_position FROM replica_review_progress WHERE pack_id = ?1",
                [pack_id],
                |row| {
                    let value = row.get::<_, i64>(0)?;
                    u32::try_from(value)
                        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, value))
                },
            )
            .map_err(LocalReplicaError::Storage)
    }

    fn immediate(&mut self) -> Result<Transaction<'_>, LocalReplicaError> {
        self.connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(LocalReplicaError::Storage)
    }
}

fn existing_device_id(connection: &Connection) -> Result<String, LocalReplicaError> {
    connection
        .query_row(
            "SELECT device_id FROM replica_identity WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .map_err(LocalReplicaError::Storage)
}

fn pending_or_accepted_asset(
    connection: &Connection,
    server_id: &str,
    logical_path: &str,
) -> Result<bool, LocalReplicaError> {
    connection
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM replica_asset_operation AS operation
                 JOIN replica_device_operation AS device USING(operation_id)
                 JOIN replica_blob AS blob USING(content_sha256)
                 LEFT JOIN replica_asset_result AS result USING(operation_id)
                 WHERE operation.logical_path = ?1 AND device.server_id = ?2
                   AND (result.operation_id IS NULL OR result.status = 'accepted')
             )",
            params![logical_path, server_id],
            |row| row.get(0),
        )
        .map_err(LocalReplicaError::Storage)
}

fn decode_stored_pack(
    stored: (String, i64, String),
) -> Result<StoredReviewPack, LocalReplicaError> {
    Ok(StoredReviewPack {
        pack: serde_json::from_str(&stored.0).map_err(|_| LocalReplicaError::Corrupt)?,
        local_position: u32::try_from(stored.1).map_err(|_| LocalReplicaError::Corrupt)?,
        status: stored.2,
    })
}

fn initialize(connection: &mut Connection, device_id: &str) -> Result<(), LocalReplicaError> {
    if device_id.is_empty() {
        return Err(LocalReplicaError::Device);
    }
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(LocalReplicaError::Storage)?;
    let version = transaction
        .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
        .map_err(LocalReplicaError::Storage)?;
    let application_id = transaction
        .pragma_query_value(None, "application_id", |row| row.get::<_, u32>(0))
        .map_err(LocalReplicaError::Storage)?;
    match (version, application_id) {
        (SCHEMA_VERSION, APPLICATION_ID) => {}
        (0, 0) if user_schema_is_empty(&transaction)? => transaction
            .execute_batch(CURRENT_SCHEMA)
            .map_err(LocalReplicaError::Storage)?,
        _ => return Err(LocalReplicaError::Schema),
    }
    let stored = transaction
        .query_row(
            "SELECT device_id FROM replica_identity WHERE id = 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(LocalReplicaError::Storage)?;
    let result = match stored {
        Some(stored) if stored != device_id => Err(LocalReplicaError::Device),
        Some(_) if sync_singleton_exists(&transaction)? => Ok(()),
        Some(_) => Err(LocalReplicaError::Corrupt),
        None => {
            transaction
                .execute(
                    "INSERT INTO replica_identity(id, device_id) VALUES (1, ?1)",
                    [device_id],
                )
                .map_err(LocalReplicaError::Storage)?;
            transaction
                .execute(
                    "INSERT INTO replica_sync(id, next_device_sequence) VALUES (1, 1)",
                    [],
                )
                .map_err(LocalReplicaError::Storage)?;
            Ok(())
        }
    };
    result?;
    transaction.commit().map_err(LocalReplicaError::Storage)
}

fn user_schema_is_empty(connection: &Connection) -> Result<bool, LocalReplicaError> {
    connection
        .query_row(
            "SELECT NOT EXISTS(
                 SELECT 1 FROM sqlite_schema
                 WHERE name NOT LIKE 'sqlite_%'
             )",
            [],
            |row| row.get(0),
        )
        .map_err(LocalReplicaError::Storage)
}

fn sync_singleton_exists(transaction: &Transaction<'_>) -> Result<bool, LocalReplicaError> {
    transaction
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM replica_sync WHERE id = 1)",
            [],
            |row| row.get(0),
        )
        .map_err(LocalReplicaError::Storage)
}

fn replica_servers(
    connection: &Connection,
) -> Result<(Option<String>, Option<String>), LocalReplicaError> {
    connection
        .query_row(
            "SELECT server_id, observed_server_id FROM replica_identity WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(LocalReplicaError::Storage)
}

fn has_pending_outbox(transaction: &Transaction<'_>) -> Result<bool, LocalReplicaError> {
    transaction
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM replica_device_operation AS operation
                 LEFT JOIN replica_review_result AS review
                   ON operation.operation_kind = 'review'
                  AND review.attempt_id = operation.operation_id
                 LEFT JOIN replica_content_result AS content
                   ON operation.operation_kind = 'content'
                  AND content.operation_id = operation.operation_id
                 LEFT JOIN replica_asset_result AS asset
                   ON asset.operation_id = operation.operation_id
                 WHERE review.attempt_id IS NULL AND content.operation_id IS NULL
                   AND asset.operation_id IS NULL
             )",
            [],
            |row| row.get(0),
        )
        .map_err(LocalReplicaError::Storage)
}

struct SyncState {
    server_id: Option<String>,
    cursor: Option<String>,
    snapshot_id: Option<String>,
    snapshot_server_id: Option<String>,
    snapshot_cut_cursor: Option<u64>,
    snapshot_cut_token: Option<String>,
    snapshot_next: Option<String>,
    snapshot_after: u64,
}

fn sync_state(transaction: &Transaction<'_>) -> Result<SyncState, LocalReplicaError> {
    transaction
        .query_row(
            "SELECT identity.server_id, sync.cursor, sync.snapshot_id,
                    sync.snapshot_server_id, sync.snapshot_cut_cursor,
                    sync.snapshot_cut_token, sync.snapshot_next, sync.snapshot_after
             FROM replica_identity AS identity
             JOIN replica_sync AS sync ON sync.id = identity.id WHERE identity.id = 1",
            [],
            |row| {
                Ok(SyncState {
                    server_id: row.get(0)?,
                    cursor: row.get(1)?,
                    snapshot_id: row.get(2)?,
                    snapshot_server_id: row.get(3)?,
                    snapshot_cut_cursor: row
                        .get::<_, Option<i64>>(4)?
                        .map(|value| {
                            u64::try_from(value)
                                .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(4, value))
                        })
                        .transpose()?,
                    snapshot_cut_token: row.get(5)?,
                    snapshot_next: row.get(6)?,
                    snapshot_after: row_u64(row, 7)?,
                })
            },
        )
        .map_err(LocalReplicaError::Storage)
}

fn replayed_snapshot_page(
    transaction: &Transaction<'_>,
    page: &SnapshotInstall,
) -> Result<bool, LocalReplicaError> {
    let request = page.requested.as_deref().unwrap_or(FIRST_REQUEST);
    let existing = transaction
        .query_row(
            "SELECT response_sha256 FROM replica_snapshot_page
             WHERE snapshot_id = ?1 AND request_cursor = ?2",
            params![page.snapshot_id, request],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(LocalReplicaError::Storage)?;
    match existing {
        Some(existing) if existing == page.response_sha256 => Ok(true),
        Some(_) => Err(LocalReplicaError::SnapshotChanged),
        None => Ok(false),
    }
}

fn prepare_snapshot(
    transaction: &Transaction<'_>,
    page: &SnapshotInstall,
) -> Result<(), LocalReplicaError> {
    let state = sync_state(transaction)?;
    if page.requested.is_some() || state.snapshot_id.as_deref() == Some(page.snapshot_id.as_str()) {
        return Ok(());
    }
    let observed = replica_servers(transaction)?.1;
    if observed.as_deref() != Some(page.server_id.as_str()) {
        return Err(LocalReplicaError::SyncOrder);
    }
    transaction
        .execute("DELETE FROM replica_snapshot_projection", [])
        .map_err(LocalReplicaError::Storage)?;
    transaction
        .execute("DELETE FROM replica_snapshot_page", [])
        .map_err(LocalReplicaError::Storage)?;
    transaction
        .execute(
            "UPDATE replica_sync SET snapshot_id = ?1, snapshot_server_id = ?2,
                 snapshot_cut_cursor = ?3, snapshot_cut_token = ?4, snapshot_next = NULL,
                 snapshot_after = 0 WHERE id = 1",
            params![
                page.snapshot_id,
                page.server_id,
                u64_to_i64(page.cut_cursor)?,
                page.cut_token,
            ],
        )
        .map_err(LocalReplicaError::Storage)?;
    Ok(())
}

fn validate_snapshot_continuation(
    transaction: &Transaction<'_>,
    page: &SnapshotInstall,
) -> Result<(), LocalReplicaError> {
    let state = sync_state(transaction)?;
    let item_count = u64::try_from(page.items.len()).map_err(|_| LocalReplicaError::Corrupt)?;
    let next_after = page
        .after
        .checked_add(item_count)
        .ok_or(LocalReplicaError::Corrupt)?;
    let expected_request = state.snapshot_next.as_deref();
    if state.snapshot_id.as_deref() != Some(page.snapshot_id.as_str())
        || state.snapshot_server_id.as_deref() != Some(page.server_id.as_str())
        || state.snapshot_cut_cursor != Some(page.cut_cursor)
        || state.snapshot_cut_token.as_deref() != Some(page.cut_token.as_str())
        || expected_request != page.requested.as_deref()
        || state.snapshot_after != page.after
        || page.next_after.is_some_and(|after| after != next_after)
        || (page.next.is_some() != page.next_after.is_some())
        || (page.next.is_none() != page.cursor.is_some())
    {
        return Err(LocalReplicaError::SyncOrder);
    }
    Ok(())
}

fn insert_snapshot_items(
    transaction: &Transaction<'_>,
    page: &SnapshotInstall,
) -> Result<(), LocalReplicaError> {
    for (offset, item) in page.items.iter().enumerate() {
        transaction
            .execute(
                "INSERT INTO replica_snapshot_projection(
                     snapshot_id, ordinal, entity_kind, entity_id, revision, tombstone, payload_json
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    page.snapshot_id,
                    u64_to_i64(
                        page.after
                            .checked_add(
                                u64::try_from(offset).map_err(|_| LocalReplicaError::Corrupt)?,
                            )
                            .ok_or(LocalReplicaError::Corrupt)?,
                    )?,
                    item.entity_kind,
                    item.entity_id,
                    u64_to_i64(item.revision)?,
                    i64::from(item.tombstone),
                    item.payload_json,
                ],
            )
            .map_err(|_| LocalReplicaError::Projection)?;
        for digest in &item.blob_digests {
            transaction
                .execute(
                    "INSERT INTO replica_snapshot_blob(
                         snapshot_id, entity_kind, entity_id, content_sha256
                     ) VALUES (?1, ?2, ?3, ?4)",
                    params![page.snapshot_id, item.entity_kind, item.entity_id, digest],
                )
                .map_err(LocalReplicaError::Storage)?;
        }
    }
    Ok(())
}

fn record_snapshot_page(
    transaction: &Transaction<'_>,
    page: &SnapshotInstall,
) -> Result<(), LocalReplicaError> {
    transaction
        .execute(
            "INSERT INTO replica_snapshot_page(
                 snapshot_id, request_cursor, response_sha256, next_cursor, terminal_cursor
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                page.snapshot_id,
                page.requested.as_deref().unwrap_or(FIRST_REQUEST),
                page.response_sha256,
                page.next,
                page.cursor,
            ],
        )
        .map_err(LocalReplicaError::Storage)?;
    Ok(())
}

fn advance_snapshot(
    transaction: &Transaction<'_>,
    page: &SnapshotInstall,
) -> Result<(), LocalReplicaError> {
    transaction
        .execute(
            "UPDATE replica_sync SET snapshot_next = ?1, snapshot_after = ?2 WHERE id = 1",
            params![
                page.next,
                u64_to_i64(page.next_after.ok_or(LocalReplicaError::SyncOrder)?)?
            ],
        )
        .map_err(LocalReplicaError::Storage)?;
    Ok(())
}

fn promote_snapshot(
    transaction: &Transaction<'_>,
    page: &SnapshotInstall,
) -> Result<(), LocalReplicaError> {
    transaction
        .execute("DELETE FROM replica_projection", [])
        .map_err(LocalReplicaError::Storage)?;
    transaction
        .execute(
            "INSERT INTO replica_projection(
                 entity_kind, entity_id, revision, tombstone, payload_json
             ) SELECT entity_kind, entity_id, revision, tombstone, payload_json
               FROM replica_snapshot_projection WHERE snapshot_id = ?1 ORDER BY ordinal",
            [&page.snapshot_id],
        )
        .map_err(LocalReplicaError::Storage)?;
    transaction
        .execute(
            "INSERT INTO replica_projection_blob(entity_kind, entity_id, content_sha256)
             SELECT entity_kind, entity_id, content_sha256 FROM replica_snapshot_blob
             WHERE snapshot_id = ?1",
            [&page.snapshot_id],
        )
        .map_err(LocalReplicaError::Storage)?;
    transaction
        .execute(
            "UPDATE replica_identity SET server_id = ?1 WHERE id = 1",
            [&page.server_id],
        )
        .map_err(LocalReplicaError::Storage)?;
    transaction
        .execute(
            "UPDATE replica_sync SET cursor = ?1, snapshot_id = NULL,
                 snapshot_server_id = NULL, snapshot_cut_cursor = NULL,
                 snapshot_cut_token = NULL, snapshot_next = NULL, snapshot_after = 0
             WHERE id = 1",
            [page.cursor.as_deref().ok_or(LocalReplicaError::SyncOrder)?],
        )
        .map_err(LocalReplicaError::Storage)?;
    transaction
        .execute("DELETE FROM replica_snapshot_projection", [])
        .map_err(LocalReplicaError::Storage)?;
    Ok(())
}

fn install_projection(
    transaction: &Transaction<'_>,
    item: &LocalReplicaItem,
) -> Result<(), LocalReplicaError> {
    let existing = transaction
        .query_row(
            "SELECT revision, tombstone, payload_json FROM replica_projection
             WHERE entity_kind = ?1 AND entity_id = ?2",
            params![item.entity_kind, item.entity_id],
            |row| {
                Ok((
                    row_u64(row, 0)?,
                    row.get::<_, i64>(1)? != 0,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
        .map_err(LocalReplicaError::Storage)?;
    if let Some((revision, tombstone, json)) = existing
        && revision >= item.revision
    {
        return if revision == item.revision
            && tombstone == item.tombstone
            && json == item.payload_json
        {
            Ok(())
        } else {
            Err(LocalReplicaError::Projection)
        };
    }
    transaction
        .execute(
            "INSERT INTO replica_projection(
                 entity_kind, entity_id, revision, tombstone, payload_json
             ) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(entity_kind, entity_id) DO UPDATE SET
                 revision = excluded.revision, tombstone = excluded.tombstone,
                 payload_json = excluded.payload_json",
            params![
                item.entity_kind,
                item.entity_id,
                u64_to_i64(item.revision)?,
                i64::from(item.tombstone),
                item.payload_json,
            ],
        )
        .map_err(LocalReplicaError::Storage)?;
    transaction
        .execute(
            "DELETE FROM replica_projection_blob WHERE entity_kind = ?1 AND entity_id = ?2",
            params![item.entity_kind, item.entity_id],
        )
        .map_err(LocalReplicaError::Storage)?;
    for digest in &item.blob_digests {
        transaction
            .execute(
                "INSERT INTO replica_projection_blob(entity_kind, entity_id, content_sha256)
                 VALUES (?1, ?2, ?3)",
                params![item.entity_kind, item.entity_id, digest],
            )
            .map_err(LocalReplicaError::Storage)?;
    }
    Ok(())
}

fn validate_pack_owner(
    transaction: &Transaction<'_>,
    pack: &ReviewPackView,
) -> Result<(), LocalReplicaError> {
    let (device, active, observed) = transaction
        .query_row(
            "SELECT device_id, server_id, observed_server_id FROM replica_identity WHERE id = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .map_err(LocalReplicaError::Storage)?;
    let server = active.as_deref().or(observed.as_deref());
    if device != pack.device_id || server != Some(pack.cursor_server_id.as_str()) {
        return Err(LocalReplicaError::Device);
    }
    Ok(())
}

fn stored_pack_json(
    transaction: &Transaction<'_>,
    pack_id: &str,
) -> Result<Option<String>, LocalReplicaError> {
    transaction
        .query_row(
            "SELECT pack_json FROM replica_review_pack WHERE pack_id = ?1",
            [pack_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(LocalReplicaError::Storage)
}

fn apply_existing_pack_update(
    transaction: &Transaction<'_>,
    existing: &str,
    replacement: &ReviewPackView,
) -> Result<bool, LocalReplicaError> {
    let mut expected: ReviewPackView =
        serde_json::from_str(existing).map_err(|_| LocalReplicaError::Corrupt)?;
    expected.status = replacement.status.clone();
    expected.current_position = replacement.current_position;
    if expected != *replacement {
        return Ok(false);
    }
    match replacement.status.as_str() {
        "active" => Ok(true),
        "abandoned" => update_pack_status(transaction, replacement, "abandoned").map(|()| true),
        "completed" if local_pack_status(transaction, &replacement.pack_id)? == "completed" => {
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn local_pack_status(
    transaction: &Transaction<'_>,
    pack_id: &str,
) -> Result<String, LocalReplicaError> {
    transaction
        .query_row(
            "SELECT status FROM replica_review_progress WHERE pack_id = ?1",
            [pack_id],
            |row| row.get(0),
        )
        .map_err(LocalReplicaError::Storage)
}

fn update_pack_status(
    transaction: &Transaction<'_>,
    pack: &ReviewPackView,
    status: &str,
) -> Result<(), LocalReplicaError> {
    transaction
        .execute(
            "UPDATE replica_review_progress SET status = ?2 WHERE pack_id = ?1",
            params![pack.pack_id, status],
        )
        .map_err(LocalReplicaError::Storage)?;
    Ok(())
}

fn insert_review_pack(
    transaction: &Transaction<'_>,
    pack: &ReviewPackView,
    cursor: &str,
    json: &str,
) -> Result<(), LocalReplicaError> {
    transaction
        .execute(
            "INSERT INTO replica_review_pack(
                 pack_id, session_id, device_id, server_id, cursor, pack_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                pack.pack_id,
                pack.session_id,
                pack.device_id,
                pack.cursor_server_id,
                cursor,
                json,
            ],
        )
        .map_err(LocalReplicaError::Storage)?;
    transaction
        .execute(
            "INSERT INTO replica_review_progress(pack_id, local_position, status)
             VALUES (?1, ?2, ?3)",
            params![pack.pack_id, pack.current_position, pack.status],
        )
        .map_err(LocalReplicaError::Storage)?;
    for asset in &pack.assets {
        transaction
            .execute(
                "INSERT INTO replica_review_pack_asset(
                     pack_id, logical_path, content_sha256, size_bytes, media_type
                 ) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    pack.pack_id,
                    asset.logical_path,
                    asset.content_sha256,
                    u64_to_i64(asset.size_bytes)?,
                    asset.media_type,
                ],
            )
            .map_err(LocalReplicaError::Storage)?;
    }
    Ok(())
}

fn require_pack_assets(
    transaction: &Transaction<'_>,
    pack_id: &str,
) -> Result<(), LocalReplicaError> {
    let missing: bool = transaction
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM replica_review_pack_asset AS asset
                 LEFT JOIN replica_blob AS blob USING(content_sha256)
                 WHERE asset.pack_id = ?1
                   AND (blob.content_sha256 IS NULL OR blob.size_bytes <> asset.size_bytes)
             )",
            [pack_id],
            |row| row.get(0),
        )
        .map_err(LocalReplicaError::Storage)?;
    if missing {
        Err(LocalReplicaError::MissingAssets)
    } else {
        Ok(())
    }
}

fn require_current_pack_server(
    transaction: &Transaction<'_>,
    pack_id: &str,
) -> Result<(), LocalReplicaError> {
    let matches: bool = transaction
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM replica_review_pack AS pack
                 JOIN replica_identity AS identity ON identity.id = 1
                 WHERE pack.pack_id = ?1
                   AND pack.server_id = identity.server_id
                   AND pack.server_id = identity.observed_server_id
             )",
            [pack_id],
            |row| row.get(0),
        )
        .map_err(LocalReplicaError::Storage)?;
    if matches {
        Ok(())
    } else {
        Err(LocalReplicaError::Answer(OfflineAnswerError::Pack))
    }
}

fn replayed_answer(
    transaction: &Transaction<'_>,
    pack_id: &str,
    input: &OfflineAnswerInput,
) -> Result<Option<OfflineReviewOperation>, LocalReplicaError> {
    let json = transaction
        .query_row(
            "SELECT operation_json FROM replica_review_operation WHERE attempt_id = ?1",
            [&input.attempt_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(LocalReplicaError::Storage)?;
    let Some(json) = json else {
        return Ok(None);
    };
    let operation: OfflineReviewOperation =
        serde_json::from_str(&json).map_err(|_| LocalReplicaError::Corrupt)?;
    let accepted_matches = input
        .accepted_success
        .map_or(operation.detected_success, |accepted| accepted)
        == operation.accepted_success;
    if operation.pack_id == pack_id
        && operation.session_position == input.session_position
        && operation.response == input.response
        && accepted_matches
        && operation.desired_suspended == input.desired_suspended
        && operation.answered_at_us == input.answered_at_us
    {
        Ok(Some(operation))
    } else {
        Err(LocalReplicaError::Answer(OfflineAnswerError::Input))
    }
}

fn load_pack_for_answer(
    transaction: &Transaction<'_>,
    pack_id: &str,
) -> Result<(ReviewPackView, u32), LocalReplicaError> {
    let (json, position, status) = transaction
        .query_row(
            "SELECT pack.pack_json, progress.local_position, progress.status
             FROM replica_review_pack AS pack
             JOIN replica_review_progress AS progress USING(pack_id)
             WHERE pack.pack_id = ?1",
            [pack_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .map_err(LocalReplicaError::Storage)?;
    if status != "active" {
        return Err(LocalReplicaError::Answer(OfflineAnswerError::Pack));
    }
    let pack = serde_json::from_str(&json).map_err(|_| LocalReplicaError::Corrupt)?;
    let position = u32::try_from(position).map_err(|_| LocalReplicaError::Corrupt)?;
    Ok((pack, position))
}

fn next_device_sequence(transaction: &Transaction<'_>) -> Result<u64, LocalReplicaError> {
    transaction
        .query_row(
            "SELECT next_device_sequence FROM replica_sync WHERE id = 1",
            [],
            |row| row_u64(row, 0),
        )
        .map_err(LocalReplicaError::Storage)
}

fn insert_review_operation(
    transaction: &Transaction<'_>,
    operation: &OfflineReviewOperation,
) -> Result<(), LocalReplicaError> {
    let json = serde_json::to_string(operation).map_err(|_| LocalReplicaError::Corrupt)?;
    let device_changed = transaction
        .execute(
            "INSERT INTO replica_device_operation(
                 operation_id, server_id, device_sequence, operation_kind, operation_json
             ) SELECT ?1, pack.server_id, ?2, 'review', ?3
               FROM replica_review_pack AS pack WHERE pack.pack_id = ?4",
            params![
                operation.attempt_id,
                u64_to_i64(operation.device_sequence)?,
                json,
                operation.pack_id,
            ],
        )
        .map_err(LocalReplicaError::Storage)?;
    let review_changed = transaction
        .execute(
            "INSERT INTO replica_review_operation(
                 attempt_id, server_id, device_sequence, pack_id,
                 session_position, operation_json
             ) SELECT ?1, pack.server_id, ?2, ?3, ?4, ?5
               FROM replica_review_pack AS pack WHERE pack.pack_id = ?3",
            params![
                operation.attempt_id,
                u64_to_i64(operation.device_sequence)?,
                operation.pack_id,
                operation.session_position,
                json,
            ],
        )
        .map_err(LocalReplicaError::Storage)?;
    if device_changed == 1 && review_changed == 1 {
        Ok(())
    } else {
        Err(LocalReplicaError::Corrupt)
    }
}

fn advance_review_progress(
    transaction: &Transaction<'_>,
    pack: &ReviewPackView,
    position: u32,
) -> Result<(), LocalReplicaError> {
    let next = position.checked_add(1).ok_or(LocalReplicaError::Corrupt)?;
    let status =
        if usize::try_from(next).map_err(|_| LocalReplicaError::Corrupt)? >= pack.items.len() {
            "completed"
        } else {
            "active"
        };
    let changed = transaction
        .execute(
            "UPDATE replica_review_progress SET local_position = ?2, status = ?3
             WHERE pack_id = ?1 AND local_position = ?4 AND status = 'active'",
            params![pack.pack_id, next, status, position],
        )
        .map_err(LocalReplicaError::Storage)?;
    if changed == 1 {
        Ok(())
    } else {
        Err(LocalReplicaError::SyncOrder)
    }
}

fn current_content_base(
    connection: &Connection,
    document_id: &str,
) -> Result<ContentBase, LocalReplicaError> {
    let stored = connection
        .query_row(
            "SELECT revision, payload_json FROM replica_projection
             WHERE entity_kind = 'document' AND entity_id = ?1",
            [document_id],
            |row| Ok((row_u64(row, 0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(LocalReplicaError::Storage)?;
    let Some((revision, payload)) = stored else {
        return Ok(ContentBase::Absent);
    };
    Ok(ContentBase::Revision {
        revision,
        content_sha256: replica_document_digest(&payload)?,
        predecessor_operation_id: None,
    })
}

fn working_content_base(
    connection: &Connection,
    server_id: &str,
    document_id: &str,
) -> Result<ContentBase, LocalReplicaError> {
    if let Some(base) = latest_chained_content_base(connection, server_id, document_id, false)? {
        return Ok(base);
    }
    let canonical = current_content_base(connection, document_id)?;
    let accepted = latest_chained_content_base(connection, server_id, document_id, true)?;
    Ok(accepted
        .filter(|base| content_base_revision(base) > content_base_revision(&canonical))
        .unwrap_or(canonical))
}

fn latest_chained_content_base(
    connection: &Connection,
    server_id: &str,
    document_id: &str,
    accepted: bool,
) -> Result<Option<ContentBase>, LocalReplicaError> {
    connection
        .query_row(
            "SELECT content.mutation, content.base_kind, content.base_revision,
                    content.content_sha256, content.base_content_sha256,
                    content.operation_id
             FROM replica_device_operation AS device
             JOIN replica_content_operation AS content USING(operation_id)
             LEFT JOIN replica_content_result AS result USING(operation_id)
             WHERE device.server_id = ?1 AND content.document_id = ?2
               AND ((?3 = 0 AND result.operation_id IS NULL)
                    OR (?3 = 1 AND result.status = 'accepted'))
             ORDER BY device.device_sequence DESC LIMIT 1",
            params![server_id, document_id, i64::from(accepted)],
            chained_content_base_row,
        )
        .optional()
        .map_err(LocalReplicaError::Storage)
}

fn chained_content_base_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ContentBase> {
    let revision = match row.get::<_, String>(1)?.as_str() {
        "absent" => 1,
        "revision" => row_u64(row, 2)?
            .checked_add(1)
            .ok_or(rusqlite::Error::IntegralValueOutOfRange(2, i64::MAX))?,
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    let digest = match row.get::<_, String>(0)?.as_str() {
        "put_document" => row.get(3)?,
        "rename_document" => row.get(4)?,
        "tombstone_document" => None,
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    Ok(ContentBase::Revision {
        revision,
        content_sha256: digest,
        predecessor_operation_id: Some(row.get(5)?),
    })
}

fn replica_document_digest(payload: &str) -> Result<Option<String>, LocalReplicaError> {
    let value: Value = serde_json::from_str(payload).map_err(|_| LocalReplicaError::Corrupt)?;
    for pointer in [
        "/projection/content_sha256",
        "/projection/projection/content_sha256",
    ] {
        match value.pointer(pointer) {
            Some(Value::String(digest)) => return Ok(Some(digest.clone())),
            Some(Value::Null) | None => {}
            Some(_) => return Err(LocalReplicaError::Corrupt),
        }
    }
    Ok(None)
}

fn replica_document_path(payload: &str) -> Result<String, LocalReplicaError> {
    let value: Value = serde_json::from_str(payload).map_err(|_| LocalReplicaError::Corrupt)?;
    value
        .pointer("/projection/logical_path")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or(LocalReplicaError::Corrupt)
}

struct WorkingDocument {
    revision: u64,
    logical_path: Option<String>,
}

fn working_documents(
    connection: &Connection,
    server_id: &str,
) -> Result<BTreeMap<String, WorkingDocument>, LocalReplicaError> {
    let mut documents = canonical_documents(connection)?;
    let mut statement = connection
        .prepare(
            "SELECT content.document_id, content.mutation, content.logical_path,
                    content.base_kind, content.base_revision
             FROM replica_device_operation AS device
             JOIN replica_content_operation AS content USING(operation_id)
             LEFT JOIN replica_content_result AS result USING(operation_id)
             WHERE device.server_id = ?1
               AND (result.operation_id IS NULL OR result.status = 'accepted')
             ORDER BY device.device_sequence",
        )
        .map_err(LocalReplicaError::Storage)?;
    let rows = statement
        .query_map([server_id], working_document_operation_row)
        .map_err(LocalReplicaError::Storage)?;
    for row in rows {
        apply_working_document(&mut documents, row.map_err(LocalReplicaError::Storage)?)?;
    }
    Ok(documents)
}

fn canonical_documents(
    connection: &Connection,
) -> Result<BTreeMap<String, WorkingDocument>, LocalReplicaError> {
    let mut statement = connection
        .prepare(
            "SELECT entity_id, revision, tombstone, payload_json FROM replica_projection
             WHERE entity_kind = 'document' ORDER BY entity_id",
        )
        .map_err(LocalReplicaError::Storage)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row_u64(row, 1)?,
                row.get::<_, i64>(2)? != 0,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(LocalReplicaError::Storage)?;
    let mut documents = BTreeMap::new();
    for row in rows {
        let (document_id, revision, tombstone, payload) =
            row.map_err(LocalReplicaError::Storage)?;
        let logical_path = (!tombstone)
            .then(|| replica_document_path(&payload))
            .transpose()?;
        documents.insert(
            document_id,
            WorkingDocument {
                revision,
                logical_path,
            },
        );
    }
    Ok(documents)
}

struct WorkingDocumentOperation {
    document_id: String,
    mutation: String,
    logical_path: Option<String>,
    revision: u64,
}

fn working_document_operation_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<WorkingDocumentOperation> {
    let revision = match row.get::<_, String>(3)?.as_str() {
        "absent" => 1,
        "revision" => row_u64(row, 4)?
            .checked_add(1)
            .ok_or(rusqlite::Error::IntegralValueOutOfRange(4, i64::MAX))?,
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    Ok(WorkingDocumentOperation {
        document_id: row.get(0)?,
        mutation: row.get(1)?,
        logical_path: row.get(2)?,
        revision,
    })
}

fn apply_working_document(
    documents: &mut BTreeMap<String, WorkingDocument>,
    operation: WorkingDocumentOperation,
) -> Result<(), LocalReplicaError> {
    let current = documents
        .entry(operation.document_id)
        .or_insert(WorkingDocument {
            revision: 0,
            logical_path: None,
        });
    if operation.revision <= current.revision {
        return Ok(());
    }
    current.logical_path = match operation.mutation.as_str() {
        "put_document" | "rename_document" => {
            Some(operation.logical_path.ok_or(LocalReplicaError::Corrupt)?)
        }
        "tombstone_document" => None,
        _ => return Err(LocalReplicaError::Corrupt),
    };
    current.revision = operation.revision;
    Ok(())
}

fn require_working_path_available(
    connection: &Connection,
    server_id: &str,
    operation: &OfflineContentOperation,
) -> Result<(), LocalReplicaError> {
    let Some(logical_path) = operation.logical_path() else {
        return Ok(());
    };
    let wanted = path_key(logical_path);
    let occupied =
        working_documents(connection, server_id)?
            .into_iter()
            .any(|(document_id, document)| {
                document_id != operation.document_id()
                    && document
                        .logical_path
                        .is_some_and(|path| path_key(&path) == wanted)
            });
    if occupied {
        Err(LocalReplicaError::PathConflict)
    } else {
        Ok(())
    }
}

const fn content_base_revision(base: &ContentBase) -> u64 {
    match base {
        ContentBase::Absent => 0,
        ContentBase::Revision { revision, .. } => *revision,
    }
}

fn replayed_content_operation(
    transaction: &Transaction<'_>,
    operation: &OfflineContentOperation,
    canonical_payload: &str,
) -> Result<bool, LocalReplicaError> {
    let existing = transaction
        .query_row(
            "SELECT operation_json FROM replica_content_operation WHERE operation_id = ?1",
            [&operation.operation_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(LocalReplicaError::Storage)?;
    match existing {
        Some(existing) if existing == canonical_payload => Ok(true),
        Some(_) => Err(LocalReplicaError::Content),
        None => Ok(false),
    }
}

fn require_content_context(
    transaction: &Transaction<'_>,
    operation: &OfflineContentOperation,
    cursor: &str,
) -> Result<(), LocalReplicaError> {
    let context = transaction
        .query_row(
            "SELECT identity.device_id, identity.server_id, identity.observed_server_id,
                    sync.cursor, sync.snapshot_id, sync.next_device_sequence,
                    identity.offline_document_lifecycle
             FROM replica_identity AS identity
             JOIN replica_sync AS sync ON sync.id = identity.id WHERE identity.id = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row_u64(row, 5)?,
                    row.get::<_, i64>(6)?,
                ))
            },
        )
        .map_err(LocalReplicaError::Storage)?;
    let context_matches = context.0 == operation.device_id
        && context.1.as_deref() == Some(operation.cursor_server_id.as_str())
        && context.2 == context.1
        && context.3.as_deref() == Some(cursor)
        && context.4.is_none()
        && context.5 == operation.device_sequence;
    let server_id = context.1.as_deref().ok_or(LocalReplicaError::Content)?;
    let base = working_content_base(transaction, server_id, operation.document_id())?;
    let capability_matches =
        matches!(&operation.mutation, OfflineContentMutation::Put { .. }) || context.6 == 1;
    if context_matches && capability_matches && &base == operation.base() {
        require_working_path_available(transaction, server_id, operation)?;
        Ok(())
    } else {
        Err(LocalReplicaError::Content)
    }
}

fn insert_local_content_operation(
    transaction: &Transaction<'_>,
    operation: &OfflineContentOperation,
    canonical_payload: &str,
) -> Result<(), LocalReplicaError> {
    transaction
        .execute(
            "INSERT INTO replica_device_operation(
                 operation_id, server_id, device_sequence, operation_kind, operation_json
             ) VALUES (?1, ?2, ?3, 'content', ?4)",
            params![
                operation.operation_id,
                operation.cursor_server_id,
                u64_to_i64(operation.device_sequence)?,
                canonical_payload,
            ],
        )
        .map_err(LocalReplicaError::Storage)?;
    let (base_kind, base_revision, base_digest, predecessor) = match operation.base() {
        ContentBase::Absent => ("absent", None, None, None),
        ContentBase::Revision {
            revision,
            content_sha256,
            predecessor_operation_id,
        } => (
            "revision",
            Some(u64_to_i64(*revision)?),
            content_sha256.as_deref(),
            predecessor_operation_id.as_deref(),
        ),
    };
    transaction
        .execute(
            "INSERT INTO replica_content_operation(
                 operation_id, document_id, mutation, logical_path, content_sha256,
                 base_kind, base_revision, base_content_sha256,
                 predecessor_operation_id, operation_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                operation.operation_id,
                operation.document_id(),
                operation.mutation_kind(),
                operation.logical_path(),
                operation.submitted_content_sha256(),
                base_kind,
                base_revision,
                base_digest,
                predecessor,
                canonical_payload,
            ],
        )
        .map_err(LocalReplicaError::Storage)?;
    Ok(())
}

fn replayed_content_result(
    transaction: &Transaction<'_>,
    operation_id: &str,
    json: &str,
) -> Result<bool, LocalReplicaError> {
    let existing = transaction
        .query_row(
            "SELECT result_json FROM replica_content_result WHERE operation_id = ?1",
            [operation_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(LocalReplicaError::Storage)?;
    match existing {
        Some(existing) if existing == json => Ok(true),
        Some(_) => Err(LocalReplicaError::Result),
        None => Ok(false),
    }
}

fn local_asset_exists(
    connection: &Connection,
    server_id: &str,
    logical_path: &str,
) -> Result<bool, LocalReplicaError> {
    connection
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM replica_projection
                 WHERE entity_kind = 'asset' AND entity_id = ?1
                 UNION ALL
                 SELECT 1 FROM replica_asset_operation AS operation
                 JOIN replica_device_operation AS device USING(operation_id)
                 LEFT JOIN replica_asset_result AS result USING(operation_id)
                 WHERE operation.logical_path = ?1 AND device.server_id = ?2
                   AND (result.operation_id IS NULL OR result.status = 'accepted')
             )",
            params![logical_path, server_id],
            |row| row.get(0),
        )
        .map_err(LocalReplicaError::Storage)
}

fn replayed_asset_operation(
    transaction: &Transaction<'_>,
    operation: &OfflineAssetOperation,
    canonical_payload: &str,
    bytes: &[u8],
) -> Result<bool, LocalReplicaError> {
    let existing = transaction
        .query_row(
            "SELECT operation.operation_json, blob.bytes
             FROM replica_asset_operation AS operation
             JOIN replica_blob AS blob USING(content_sha256)
             WHERE operation.operation_id = ?1",
            [&operation.operation_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()
        .map_err(LocalReplicaError::Storage)?;
    match existing {
        Some((json, stored)) if json == canonical_payload && stored == bytes => Ok(true),
        Some(_) => Err(LocalReplicaError::Content),
        None => Ok(false),
    }
}

fn require_asset_context(
    transaction: &Transaction<'_>,
    operation: &OfflineAssetOperation,
    cursor: &str,
) -> Result<(), LocalReplicaError> {
    let context = transaction
        .query_row(
            "SELECT identity.device_id, identity.server_id, identity.observed_server_id,
                    sync.cursor, sync.snapshot_id, sync.next_device_sequence,
                    identity.offline_asset_put
             FROM replica_identity AS identity
             JOIN replica_sync AS sync ON sync.id = identity.id WHERE identity.id = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row_u64(row, 5)?,
                    row.get::<_, bool>(6)?,
                ))
            },
        )
        .map_err(LocalReplicaError::Storage)?;
    let matches = context.0 == operation.device_id
        && context.1.as_deref() == Some(operation.cursor_server_id.as_str())
        && context.2 == context.1
        && context.3.as_deref() == Some(cursor)
        && context.4.is_none()
        && context.5 == operation.device_sequence
        && context.6
        && !local_asset_exists(
            transaction,
            &operation.cursor_server_id,
            &operation.logical_path,
        )?;
    matches.then_some(()).ok_or(LocalReplicaError::Content)
}

fn install_blob_in(
    transaction: &Transaction<'_>,
    digest: &str,
    bytes: &[u8],
) -> Result<(), LocalReplicaError> {
    if format!("{:x}", Sha256::digest(bytes)) != digest {
        return Err(LocalReplicaError::Corrupt);
    }
    let existing = transaction
        .query_row(
            "SELECT bytes FROM replica_blob WHERE content_sha256 = ?1",
            [digest],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()
        .map_err(LocalReplicaError::Storage)?;
    if let Some(existing) = existing {
        return if existing == bytes {
            Ok(())
        } else {
            Err(LocalReplicaError::Corrupt)
        };
    }
    transaction
        .execute(
            "INSERT INTO replica_blob(content_sha256, size_bytes, bytes) VALUES (?1, ?2, ?3)",
            params![
                digest,
                u64_to_i64(u64::try_from(bytes.len()).map_err(|_| LocalReplicaError::Corrupt)?)?,
                bytes,
            ],
        )
        .map_err(LocalReplicaError::Storage)?;
    Ok(())
}

fn insert_local_asset_operation(
    transaction: &Transaction<'_>,
    operation: &OfflineAssetOperation,
    canonical_payload: &str,
) -> Result<(), LocalReplicaError> {
    transaction
        .execute(
            "INSERT INTO replica_device_operation(
                 operation_id, server_id, device_sequence, operation_kind, operation_json
             ) VALUES (?1, ?2, ?3, 'content', ?4)",
            params![
                operation.operation_id,
                operation.cursor_server_id,
                u64_to_i64(operation.device_sequence)?,
                canonical_payload,
            ],
        )
        .map_err(LocalReplicaError::Storage)?;
    transaction
        .execute(
            "INSERT INTO replica_asset_operation(
                 operation_id, logical_path, content_sha256, size_bytes,
                 media_type, operation_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                operation.operation_id,
                operation.logical_path,
                operation.content_sha256,
                u64_to_i64(operation.size_bytes)?,
                operation.media_type,
                canonical_payload,
            ],
        )
        .map_err(LocalReplicaError::Storage)?;
    Ok(())
}

fn replayed_asset_result(
    transaction: &Transaction<'_>,
    operation_id: &str,
    json: &str,
) -> Result<bool, LocalReplicaError> {
    let existing = transaction
        .query_row(
            "SELECT result_json FROM replica_asset_result WHERE operation_id = ?1",
            [operation_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(LocalReplicaError::Storage)?;
    match existing {
        Some(existing) if existing == json => Ok(true),
        Some(_) => Err(LocalReplicaError::Result),
        None => Ok(false),
    }
}

fn require_next_asset_result(
    transaction: &Transaction<'_>,
    result: &AssetResultInstall,
) -> Result<(), LocalReplicaError> {
    let Some((kind, operation_id, server_id, logical_path)) =
        next_unresolved_device_operation(transaction)?
    else {
        return Err(LocalReplicaError::Result);
    };
    let status_matches = match result.status {
        AssetReconciliationStatus::Accepted => {
            result.conflict_reason.is_none() && result.authoritative_path.is_some()
        }
        AssetReconciliationStatus::Conflict => result.conflict_reason.is_some(),
    };
    let authority_matches = result
        .authoritative_path
        .as_deref()
        .is_none_or(|path| path == logical_path);
    if kind == "asset"
        && operation_id == result.operation_id
        && server_id == result.cursor_server_id
        && status_matches
        && authority_matches
    {
        Ok(())
    } else {
        Err(LocalReplicaError::Result)
    }
}

fn require_next_content_result(
    transaction: &Transaction<'_>,
    result: &ContentResultInstall,
) -> Result<(), LocalReplicaError> {
    let next = next_unresolved_device_operation(transaction)?;
    let Some((kind, operation_id, server_id, document_id)) = next else {
        return Err(LocalReplicaError::Result);
    };
    let status_matches = match result.status {
        ContentReconciliationStatus::Accepted => {
            result.conflict_reason.is_none() && result.authoritative_document_id.is_some()
        }
        ContentReconciliationStatus::Conflict => result.conflict_reason.is_some(),
    };
    let authoritative_matches = result
        .authoritative_document_id
        .as_deref()
        .is_none_or(|id| id == document_id);
    if kind == "document"
        && operation_id == result.operation_id
        && server_id == result.cursor_server_id
        && status_matches
        && authoritative_matches
    {
        Ok(())
    } else {
        Err(LocalReplicaError::Result)
    }
}

type UnresolvedDeviceOperation = (String, String, String, String);

fn next_unresolved_device_operation(
    transaction: &Transaction<'_>,
) -> Result<Option<UnresolvedDeviceOperation>, LocalReplicaError> {
    transaction
        .query_row(
            "SELECT CASE
                        WHEN review_operation.attempt_id IS NOT NULL THEN 'review'
                        WHEN content.operation_id IS NOT NULL THEN 'document'
                        WHEN asset.operation_id IS NOT NULL THEN 'asset'
                        ELSE 'invalid'
                    END,
                    device.operation_id, device.server_id,
                    coalesce(content.document_id, asset.logical_path, '')
             FROM replica_device_operation AS device
             LEFT JOIN replica_review_operation AS review_operation
               ON review_operation.attempt_id = device.operation_id
             LEFT JOIN replica_review_result AS review
               ON review.attempt_id = device.operation_id
             LEFT JOIN replica_content_operation AS content
               ON content.operation_id = device.operation_id
             LEFT JOIN replica_content_result AS result
               ON result.operation_id = device.operation_id
             LEFT JOIN replica_asset_operation AS asset
               ON asset.operation_id = device.operation_id
             LEFT JOIN replica_asset_result AS asset_result
               ON asset_result.operation_id = device.operation_id
             WHERE review.attempt_id IS NULL AND result.operation_id IS NULL
               AND asset_result.operation_id IS NULL
             ORDER BY device.device_sequence LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .map_err(LocalReplicaError::Storage)
}

fn load_content_results(
    connection: &Connection,
    after: u64,
    limit: u16,
    conflicts_only: bool,
) -> Result<StoredContentResultPage, LocalReplicaError> {
    let mut statement = connection
        .prepare(
            "SELECT result.result_sequence, device.server_id,
                    content.operation_json, result.result_json
             FROM replica_content_operation AS content
             JOIN replica_device_operation AS device USING(operation_id)
             JOIN replica_content_result AS result USING(operation_id)
             WHERE result.result_sequence > ?1
               AND (?2 = 0 OR result.status = 'conflict')
             ORDER BY result.result_sequence LIMIT ?3",
        )
        .map_err(LocalReplicaError::Storage)?;
    let rows = statement
        .query_map(
            params![
                u64_to_i64(after)?,
                i64::from(conflicts_only),
                i64::from(limit) + 1,
            ],
            |row| {
                Ok(StoredContentResult {
                    sequence: row_u64(row, 0)?,
                    server_id: row.get(1)?,
                    operation_json: row.get(2)?,
                    result_json: row.get(3)?,
                })
            },
        )
        .map_err(LocalReplicaError::Storage)?;
    let mut items = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(LocalReplicaError::Storage)?;
    let has_more = items.len() > usize::from(limit);
    if has_more {
        items.pop();
    }
    let next_after = has_more.then(|| items.last().expect("nonzero page limit").sequence);
    Ok(StoredContentResultPage { items, next_after })
}

fn load_asset_results(
    connection: &Connection,
    after: u64,
    limit: u16,
    conflicts_only: bool,
) -> Result<StoredAssetResultPage, LocalReplicaError> {
    let mut statement = connection
        .prepare(
            "SELECT result.result_sequence, device.server_id,
                    asset.operation_json, result.result_json
             FROM replica_asset_operation AS asset
             JOIN replica_device_operation AS device USING(operation_id)
             JOIN replica_asset_result AS result USING(operation_id)
             WHERE result.result_sequence > ?1
               AND (?2 = 0 OR result.status = 'conflict')
             ORDER BY result.result_sequence LIMIT ?3",
        )
        .map_err(LocalReplicaError::Storage)?;
    let rows = statement
        .query_map(
            params![
                u64_to_i64(after)?,
                i64::from(conflicts_only),
                i64::from(limit) + 1,
            ],
            |row| {
                Ok(StoredAssetResult {
                    sequence: row_u64(row, 0)?,
                    server_id: row.get(1)?,
                    operation_json: row.get(2)?,
                    result_json: row.get(3)?,
                })
            },
        )
        .map_err(LocalReplicaError::Storage)?;
    let mut items = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(LocalReplicaError::Storage)?;
    let has_more = items.len() > usize::from(limit);
    if has_more {
        items.pop();
    }
    let next_after = has_more.then(|| items.last().expect("nonzero page limit").sequence);
    Ok(StoredAssetResultPage { items, next_after })
}

fn replayed_result(
    transaction: &Transaction<'_>,
    attempt_id: &str,
    json: &str,
) -> Result<bool, LocalReplicaError> {
    let existing = transaction
        .query_row(
            "SELECT result_json FROM replica_review_result WHERE attempt_id = ?1",
            [attempt_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(LocalReplicaError::Storage)?;
    match existing {
        Some(existing) if existing == json => Ok(true),
        Some(_) => Err(LocalReplicaError::Result),
        None => Ok(false),
    }
}

fn require_next_result(
    transaction: &Transaction<'_>,
    result: &OfflineReviewResult,
) -> Result<(), LocalReplicaError> {
    let server_id = replica_servers(transaction)?.0;
    let pending =
        next_unresolved_device_operation(transaction)?.ok_or(LocalReplicaError::Result)?;
    if pending.0 != "review" || pending.1 != result.attempt_id {
        return Err(LocalReplicaError::Result);
    }
    let next = transaction
        .query_row(
            "SELECT operation.attempt_id, operation.operation_json, pack.pack_json
             FROM replica_review_operation AS operation
             JOIN replica_review_pack AS pack USING(pack_id)
             WHERE operation.attempt_id = ?1",
            [&result.attempt_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .map_err(LocalReplicaError::Storage)?;
    let operation: OfflineReviewOperation =
        serde_json::from_str(&next.1).map_err(|_| LocalReplicaError::Corrupt)?;
    let pack: ReviewPackView =
        serde_json::from_str(&next.2).map_err(|_| LocalReplicaError::Corrupt)?;
    let queue_length = u32::try_from(pack.items.len()).map_err(|_| LocalReplicaError::Corrupt)?;
    let status_matches = match result.status {
        ReconciliationStatus::Accepted => {
            let mut expected = operation.proposed_transition.after.clone();
            expected.suspended = operation.desired_suspended;
            let next_position = operation.session_position.checked_add(1);
            let expected_status = if next_position == Some(queue_length) {
                "completed"
            } else {
                "active"
            };
            result.conflict_reason.is_none()
                && result.authoritative_state == expected
                && next_position == Some(result.session.current_position)
                && result.session.status == expected_status
        }
        ReconciliationStatus::Conflict => result.conflict_reason.is_some(),
    };
    if next.0 == result.attempt_id
        && operation.attempt_id == result.attempt_id
        && operation.session_id == result.session.session_id
        && result.session.mode == ReviewSessionMode::Offline
        && result.session.owner_device_id.as_deref() == Some(operation.device_id.as_str())
        && result.session.queue_length == queue_length
        && server_id.as_deref() == Some(result.cursor_server_id.as_str())
        && status_matches
    {
        Ok(())
    } else {
        Err(LocalReplicaError::Result)
    }
}

#[cfg(test)]
fn count(connection: &Connection, table: &str) -> Result<u64, LocalReplicaError> {
    let sql = format!("SELECT count(*) FROM {table}");
    connection
        .query_row(&sql, [], |row| row_u64(row, 0))
        .map_err(LocalReplicaError::Storage)
}

fn load_projection_blobs(
    connection: &Connection,
    kind: &str,
    id: &str,
) -> Result<Vec<String>, LocalReplicaError> {
    let mut statement = connection
        .prepare(
            "SELECT content_sha256 FROM replica_projection_blob
             WHERE entity_kind = ?1 AND entity_id = ?2 ORDER BY content_sha256",
        )
        .map_err(LocalReplicaError::Storage)?;
    let rows = statement
        .query_map(params![kind, id], |row| row.get::<_, String>(0))
        .map_err(LocalReplicaError::Storage)?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(LocalReplicaError::Storage)
}

fn row_u64(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value = row.get::<_, i64>(index)?;
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
}

fn u64_to_i64(value: u64) -> Result<i64, LocalReplicaError> {
    i64::try_from(value).map_err(|_| LocalReplicaError::Corrupt)
}

pub(crate) fn canonical_json_sha256(value: &Value) -> Result<String, LocalReplicaError> {
    let bytes = serde_json::to_vec(value).map_err(|_| LocalReplicaError::Corrupt)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rusqlite::Connection;

    use super::{APPLICATION_ID, LocalReplicaStore, SCHEMA_VERSION};

    #[test]
    fn existing_replica_reopens_without_an_external_device_identity() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("replica.sqlite");
        drop(LocalReplicaStore::open(&path, "device").unwrap());

        let reopened = LocalReplicaStore::open_existing(&path).unwrap();

        assert_eq!(reopened.device_id().unwrap(), "device");
    }

    #[test]
    fn existing_replica_open_never_creates_a_database() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing.sqlite");

        assert!(LocalReplicaStore::open_existing(&missing).is_err());
        assert!(!missing.exists());
    }

    #[test]
    fn existing_replica_open_rejects_an_unrelated_database_unchanged() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("unrelated.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute("CREATE TABLE unrelated(value TEXT)", [])
            .unwrap();
        drop(connection);
        let before = fs::read(&path).unwrap();

        assert!(LocalReplicaStore::open_existing(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn rejects_noncurrent_replica_schema_without_changes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("replica.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE marker(value TEXT);
                 INSERT INTO marker VALUES ('preserved');
                 PRAGMA user_version = 2;",
            )
            .unwrap();
        drop(connection);

        assert!(matches!(
            LocalReplicaStore::open(&path, "device"),
            Err(super::LocalReplicaError::Schema)
        ));
        let connection = Connection::open(&path).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT value FROM marker", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "preserved"
        );
    }

    #[test]
    fn rejects_historical_replica_v1_without_the_current_identity() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("replica.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE replica_identity(
                     id INTEGER PRIMARY KEY, device_id TEXT NOT NULL
                 );
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        drop(connection);

        assert!(matches!(
            LocalReplicaStore::open(&path, "device"),
            Err(super::LocalReplicaError::Schema)
        ));
    }

    #[test]
    fn current_replica_schema_is_v1() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("replica.sqlite");
        let store = LocalReplicaStore::open(&path, "device").unwrap();

        let version: u32 = store
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        let application_id: u32 = store
            .connection
            .pragma_query_value(None, "application_id", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(application_id, APPLICATION_ID);
        for table in [
            "replica_projection",
            "replica_device_operation",
            "replica_content_operation",
            "replica_asset_operation",
        ] {
            let present: bool = store
                .connection
                .query_row(
                    "SELECT EXISTS(
                         SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1
                     )",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(present, "missing {table}");
        }
    }
}
