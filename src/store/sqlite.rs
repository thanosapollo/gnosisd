use std::{
    collections::{BTreeMap, HashSet},
    error::Error,
    fmt::Write as _,
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    time::Duration,
};

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use thiserror::Error;
use uuid::Uuid;

use crate::domain::{
    DocumentId, DocumentProjection, DocumentProjectionInput, DueThema, LegacyDatabaseInfo,
    LegacyReviewState, LegacyThema, LinkOccurrence, Node, NodeEdge, NodeId, SourceSpan, ThemaId,
    TitlePath,
};
use crate::{
    application::{
        AssetHead, AssetRevision, ContentChange, DevicePrincipal, DeviceRecord, DocumentHead,
        DocumentProjectionSnapshot, DocumentReplicaRevision, DocumentRevision, NewDevice,
        NodeRecord, PreparedAsset, PreparedNote, PreparedThema, ReplicaChange, ReplicaProjection,
        ReplicaState, ReviewStateRevision, SettingsInput, SettingsRecord, SettingsRevision,
        SnapshotPage, SnapshotPosition, TagRule, ThemaRecord, ThemaRevision, parse_stored_scopes,
        path_key, stored_scopes, stored_thema_content, validate_org_path,
    },
    blob::BlobMeta,
};

mod content_sync;
mod review;

const INITIAL_MIGRATION: &str = include_str!("migrations/0001_initial.sql");
const SERVER_MIGRATION: &str = include_str!("migrations/0002_server.sql");
const REMOTE_REVIEW_MIGRATION: &str = include_str!("migrations/0003_remote_review.sql");
const OFFLINE_REVIEW_MIGRATION: &str = include_str!("migrations/0004_offline_review.sql");
const REPLICA_SYNC_MIGRATION: &str = include_str!("migrations/0005_replica_sync.sql");
const OFFLINE_CONTENT_MIGRATION: &str = include_str!("migrations/0006_offline_content.sql");
const OFFLINE_DOCUMENT_LIFECYCLE_MIGRATION: &str =
    include_str!("migrations/0007_offline_document_lifecycle.sql");
const DOCUMENT_ASSET_INTEGRITY_MIGRATION: &str =
    include_str!("migrations/0008_document_asset_integrity.sql");
const SCHEMA_VERSION: u32 = 8;

#[derive(Debug, Error)]
pub(crate) enum StoreError {
    #[error("revision conflict: expected {expected}, current revision is {actual:?}")]
    Conflict { expected: u64, actual: Option<u64> },
    #[error("resource was not found")]
    NotFound,
    #[error("another review session is active")]
    ActiveReviewSession,
    #[error("review attempt is stale")]
    StaleAttempt,
    #[error("review operation is invalid in the current state")]
    InvalidReviewState,
    #[error("review operation belongs to another device")]
    ReviewOwnership,
    #[error("operation identity was reused with a different payload")]
    IdempotencyConflict,
    #[error("device operation sequence has a gap: expected {expected}")]
    DeviceSequence { expected: u64 },
    #[error("replica cursor is outside the retained server feed")]
    ReplicaCursor,
    #[error("replica snapshot belongs to another server")]
    ReplicaServer,
    #[error("document path changes require a rename operation")]
    PathChange,
    #[error("document projection is invalid: {reason}")]
    InvalidProjection { reason: &'static str },
    #[error("stored document contains invalid {field}")]
    CorruptData { field: &'static str },
    #[error("database schema version {found} is newer than supported version {supported}")]
    UnsupportedSchemaVersion { found: u32, supported: u32 },
    #[error("storage operation failed")]
    Storage(#[source] Box<dyn Error + Send + Sync>),
}

impl StoreError {
    fn storage(error: impl Error + Send + Sync + 'static) -> Self {
        Self::Storage(Box::new(error))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredDocument {
    document_id: DocumentId,
    revision: u64,
    projection: DocumentProjection,
}

impl StoredDocument {
    pub(crate) fn document_id(&self) -> &DocumentId {
        &self.document_id
    }

    pub(crate) const fn revision(&self) -> u64 {
        self.revision
    }

    pub(crate) fn projection(&self) -> &DocumentProjection {
        &self.projection
    }
}

pub(crate) struct DocumentAssetSource {
    pub(crate) document_id: String,
    pub(crate) revision: u64,
    pub(crate) blob_sha256: String,
    pub(crate) active_head: bool,
}

pub(crate) struct DocumentAssetAssociation {
    pub(crate) document_id: String,
    pub(crate) revision: u64,
    pub(crate) asset_paths: Vec<String>,
    pub(crate) active_head: bool,
}

pub(crate) struct SqliteStore {
    connection: Connection,
}

impl SqliteStore {
    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref();
        let connection = Connection::open(path).map_err(StoreError::storage)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(StoreError::storage)?;
        Self::initialize(connection)
    }

    #[cfg(test)]
    fn open_in_memory() -> Result<Self, StoreError> {
        Self::initialize(Connection::open_in_memory().map_err(StoreError::storage)?)
    }

    fn initialize(connection: Connection) -> Result<Self, StoreError> {
        connection
            .pragma_update(None, "foreign_keys", true)
            .map_err(StoreError::storage)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(StoreError::storage)?;
        migrate(&connection)?;
        backfill_current_replica_history(&connection)?;
        Ok(Self { connection })
    }

    #[cfg(test)]
    fn schema_version(&self) -> Result<u32, StoreError> {
        schema_version(&self.connection)
    }

    pub(crate) fn get_document(
        &self,
        document_id: &DocumentId,
    ) -> Result<Option<StoredDocument>, StoreError> {
        load_document(&self.connection, document_id)
    }

    pub(crate) fn legacy_database_info(&self) -> Result<Option<LegacyDatabaseInfo>, StoreError> {
        load_legacy_database_info(&self.connection)
    }

    pub(crate) fn get_legacy_thema(
        &self,
        thema_id: &ThemaId,
    ) -> Result<Option<LegacyThema>, StoreError> {
        load_legacy_thema(&self.connection, thema_id)
    }

    pub(crate) fn due_legacy_themata(
        &self,
        day: u32,
        limit: u16,
    ) -> Result<Vec<DueThema>, StoreError> {
        load_due_legacy_themata(&self.connection, day, limit, None)
    }

    pub(crate) fn due_legacy_themata_page(
        &self,
        day: u32,
        limit: u16,
        after: Option<(bool, u32, i64)>,
    ) -> Result<Vec<DueThema>, StoreError> {
        load_due_legacy_themata(&self.connection, day, limit, after)
    }

    #[cfg(test)]
    pub(crate) fn replace_document(
        &mut self,
        document_id: &DocumentId,
        projection: &DocumentProjection,
        expected_revision: Option<u64>,
    ) -> Result<StoredDocument, StoreError> {
        validate_projection(projection)?;
        self.replace(document_id, projection, expected_revision, FailPoint::None)
    }

    pub(crate) fn replace_note(
        &mut self,
        prepared: &PreparedNote,
        blob: &BlobMeta,
        created_at_us: i64,
        author_device_id: Option<&str>,
    ) -> Result<StoredDocument, StoreError> {
        validate_projection(&prepared.projection)?;
        if prepared.blob_sha256 != blob.sha256 {
            return Err(StoreError::CorruptData { field: "note blob" });
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(StoreError::storage)?;
        let stored = replace_note_in(
            &transaction,
            prepared,
            blob,
            prepared.expected_revision,
            created_at_us,
            author_device_id,
            true,
        )?;
        transaction.commit().map_err(StoreError::storage)?;
        Ok(stored)
    }

    pub(crate) fn document_logical_path(
        &self,
        document_id: &DocumentId,
    ) -> Result<Option<String>, StoreError> {
        document_path(&self.connection, document_id)
    }

    pub(crate) fn put_asset(
        &mut self,
        prepared: &PreparedAsset,
        blob: &BlobMeta,
        created_at_us: i64,
        author_device_id: Option<&str>,
    ) -> Result<u64, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(StoreError::storage)?;
        let collides: bool = transaction
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM managed_asset
                     WHERE path_key = ?1 AND asset_path <> ?2
                 )",
                params![prepared.path_key, prepared.logical_path],
                |row| row.get(0),
            )
            .map_err(StoreError::storage)?;
        if collides {
            return Err(StoreError::InvalidProjection {
                reason: "asset path collides by case",
            });
        }
        let current: Option<(String, i64, bool)> = transaction
            .query_row(
                "SELECT blob_sha256, revision, active
                 FROM managed_asset WHERE asset_path = ?1",
                [&prepared.logical_path],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(StoreError::storage)?;
        check_expected_number(
            prepared.expected_revision,
            current.as_ref().map(|(_, revision, _)| *revision),
        )?;
        if current
            .as_ref()
            .is_some_and(|(digest, _, active)| digest == &blob.sha256 && *active)
        {
            transaction.commit().map_err(StoreError::storage)?;
            return current
                .and_then(|(_, revision, _)| u64::try_from(revision).ok())
                .ok_or(StoreError::CorruptData {
                    field: "asset revision",
                });
        }
        let revision = next_number(current.as_ref().map(|(_, revision, _)| *revision))?;
        insert_blob(&transaction, blob, created_at_us)?;
        transaction
            .execute(
                "INSERT INTO managed_asset(
                     asset_path, path_key, blob_sha256, original_name, revision,
                     active, created_at_us, updated_at_us
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?6)
                 ON CONFLICT(asset_path) DO UPDATE SET
                     path_key = excluded.path_key,
                     blob_sha256 = excluded.blob_sha256,
                     original_name = excluded.original_name,
                     revision = excluded.revision,
                     active = 1,
                     updated_at_us = excluded.updated_at_us",
                params![
                    prepared.logical_path,
                    prepared.path_key,
                    blob.sha256,
                    prepared.original_name,
                    u64_to_i64(revision, "asset revision")?,
                    created_at_us
                ],
            )
            .map_err(StoreError::storage)?;
        transaction
            .execute(
                "INSERT INTO asset_revision(
                     asset_path, revision, blob_sha256, operation, created_at_us,
                     author_device_id
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    prepared.logical_path,
                    u64_to_i64(revision, "asset revision")?,
                    blob.sha256,
                    if current.is_some() {
                        "update"
                    } else {
                        "create"
                    },
                    created_at_us,
                    author_device_id
                ],
            )
            .map_err(StoreError::storage)?;
        insert_content_change(
            &transaction,
            "asset",
            &prepared.logical_path,
            revision,
            if current.is_some() {
                "update"
            } else {
                "create"
            },
            created_at_us,
        )?;
        transaction.commit().map_err(StoreError::storage)?;
        Ok(revision)
    }

    pub(crate) fn rewrite_imported_thema_content(
        &mut self,
        record: &ThemaRecord,
    ) -> Result<(), StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(StoreError::storage)?;
        transaction
            .execute(
                "UPDATE thema SET keimenon = ?2, hypothesis_json = ?3,
                     answer_json = ?4, parathema = ?5, review_image = ?6,
                     content_sha256 = ?7
                 WHERE thema_id = ?1",
                params![
                    record.thema_id,
                    record.content.keimenon,
                    serde_json::to_string(&record.content.hypothesis)
                        .map_err(StoreError::storage)?,
                    serde_json::to_string(&record.content.answer).map_err(StoreError::storage)?,
                    record.content.parathema,
                    record.content.review_image,
                    record.content_sha256,
                ],
            )
            .map_err(StoreError::storage)?;
        transaction
            .execute(
                "DELETE FROM thema_asset WHERE thema_id = ?1",
                [&record.thema_id],
            )
            .map_err(StoreError::storage)?;
        let mut insert = transaction
            .prepare_cached(
                "INSERT INTO thema_asset(thema_id, asset_path, role) VALUES (?1, ?2, ?3)",
            )
            .map_err(StoreError::storage)?;
        for asset_path in &record.asset_paths {
            let role = if record.content.review_image.as_deref() == Some(asset_path) {
                "review_image"
            } else {
                "content"
            };
            insert
                .execute(params![record.thema_id, asset_path, role])
                .map_err(StoreError::storage)?;
        }
        drop(insert);
        let updated = transaction
            .execute(
                "UPDATE thema_revision SET
                     kind = ?3, keimenon = ?4, hypothesis_json = ?5,
                     answer_json = ?6, source_guid = ?7, parathema = ?8,
                     review_image = ?9, tags_json = ?10, node_ids_json = ?11,
                     asset_paths_json = ?12, content_sha256 = ?13,
                     archived = ?14
                 WHERE thema_id = ?1 AND revision = ?2",
                params![
                    record.thema_id,
                    u64_to_i64(record.revision, "thema revision")?,
                    record.content.kind.as_str(),
                    record.content.keimenon,
                    serde_json::to_string(&record.content.hypothesis)
                        .map_err(StoreError::storage)?,
                    serde_json::to_string(&record.content.answer).map_err(StoreError::storage)?,
                    record.content.source_guid,
                    record.content.parathema,
                    record.content.review_image,
                    serde_json::to_string(&record.tags).map_err(StoreError::storage)?,
                    serde_json::to_string(&record.node_ids).map_err(StoreError::storage)?,
                    serde_json::to_string(&record.asset_paths).map_err(StoreError::storage)?,
                    record.content_sha256,
                    i64::from(record.archived),
                ],
            )
            .map_err(StoreError::storage)?;
        if updated != 1 {
            return Err(StoreError::CorruptData {
                field: "thema revision history",
            });
        }
        transaction.commit().map_err(StoreError::storage)
    }

    pub(crate) fn record_import_path_rewrite(
        &self,
        source_path: &str,
        logical_path: &str,
        reason: &str,
    ) -> Result<(), StoreError> {
        self.connection
            .execute(
                "INSERT OR IGNORE INTO import_path_rewrite(source_path, logical_path, reason)
                 VALUES (?1, ?2, ?3)",
                params![source_path, logical_path, reason],
            )
            .map_err(StoreError::storage)?;
        Ok(())
    }

    pub(crate) fn document_head(
        &self,
        document_id: &DocumentId,
    ) -> Result<Option<DocumentHead>, StoreError> {
        self.connection
            .query_row(
                "SELECT document_id, logical_path, revision, head_blob_sha256, active
                 FROM document WHERE document_id = ?1",
                [document_id.as_str()],
                document_head_row,
            )
            .optional()
            .map_err(StoreError::storage)
    }

    pub(crate) fn document_revisions(
        &self,
        document_id: &DocumentId,
        after: u64,
        limit: u16,
    ) -> Result<Vec<DocumentRevision>, StoreError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT document_id, revision, logical_path, blob_sha256, operation,
                        created_at_us, author_device_id
                 FROM document_revision
                 WHERE document_id = ?1 AND revision > ?2
                 ORDER BY revision LIMIT ?3",
            )
            .map_err(StoreError::storage)?;
        statement
            .query_map(
                params![
                    document_id.as_str(),
                    u64_to_i64(after, "document revision")?,
                    i64::from(limit)
                ],
                document_revision_row,
            )
            .map_err(StoreError::storage)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::storage)
    }

    pub(crate) fn document_revision(
        &self,
        document_id: &DocumentId,
        revision: u64,
    ) -> Result<Option<DocumentRevision>, StoreError> {
        self.connection
            .query_row(
                "SELECT document_id, revision, logical_path, blob_sha256, operation,
                        created_at_us, author_device_id
                 FROM document_revision WHERE document_id = ?1 AND revision = ?2",
                params![
                    document_id.as_str(),
                    u64_to_i64(revision, "document revision")?
                ],
                document_revision_row,
            )
            .optional()
            .map_err(StoreError::storage)
    }

    pub(crate) fn rename_document(
        &mut self,
        document_id: &DocumentId,
        logical_path: &str,
        expected_revision: u64,
        updated_at_us: i64,
        author_device_id: Option<&str>,
    ) -> Result<DocumentHead, StoreError> {
        validate_org_path(logical_path).map_err(|_| StoreError::InvalidProjection {
            reason: "logical path is invalid",
        })?;
        let transaction = self.write_transaction()?;
        let head = rename_document_in(
            &transaction,
            document_id,
            logical_path,
            expected_revision,
            updated_at_us,
            author_device_id,
        )?;
        transaction.commit().map_err(StoreError::storage)?;
        Ok(head)
    }

    pub(crate) fn tombstone_document(
        &mut self,
        document_id: &DocumentId,
        expected_revision: u64,
        updated_at_us: i64,
        author_device_id: Option<&str>,
    ) -> Result<DocumentHead, StoreError> {
        let transaction = self.write_transaction()?;
        let head = tombstone_document_in(
            &transaction,
            document_id,
            expected_revision,
            updated_at_us,
            author_device_id,
        )?;
        transaction.commit().map_err(StoreError::storage)?;
        Ok(head)
    }

    pub(crate) fn asset_head(&self, path: &str) -> Result<Option<AssetHead>, StoreError> {
        self.connection
            .query_row(
                "SELECT asset.asset_path, asset.revision, asset.blob_sha256,
                        blob.media_type, asset.active
                 FROM managed_asset AS asset
                 JOIN blob ON blob.content_sha256 = asset.blob_sha256
                 WHERE asset.asset_path = ?1",
                [path],
                asset_head_row,
            )
            .optional()
            .map_err(StoreError::storage)
    }

    pub(crate) fn tombstone_asset(
        &mut self,
        asset_path: &str,
        expected_revision: u64,
        updated_at_us: i64,
        author_device_id: Option<&str>,
    ) -> Result<AssetHead, StoreError> {
        let transaction = self.write_transaction()?;
        let current = load_asset_head(&transaction, asset_path)?
            .filter(|asset| asset.active)
            .ok_or(StoreError::NotFound)?;
        check_expected_number(Some(expected_revision), Some(current.revision as i64))?;
        require_asset_unreferenced(&transaction, asset_path)?;
        let revision = current
            .revision
            .checked_add(1)
            .ok_or(StoreError::CorruptData {
                field: "asset revision",
            })?;
        transaction
            .execute(
                "UPDATE managed_asset SET active = 0, revision = ?2, updated_at_us = ?3
             WHERE asset_path = ?1",
                params![
                    asset_path,
                    u64_to_i64(revision, "asset revision")?,
                    updated_at_us
                ],
            )
            .map_err(StoreError::storage)?;
        transaction
            .execute(
                "INSERT INTO asset_revision(
                 asset_path, revision, blob_sha256, operation, created_at_us, author_device_id
             ) VALUES (?1, ?2, NULL, 'tombstone', ?3, ?4)",
                params![
                    asset_path,
                    u64_to_i64(revision, "asset revision")?,
                    updated_at_us,
                    author_device_id
                ],
            )
            .map_err(StoreError::storage)?;
        insert_content_change(
            &transaction,
            "asset",
            asset_path,
            revision,
            "tombstone",
            updated_at_us,
        )?;
        transaction.commit().map_err(StoreError::storage)?;
        Ok(AssetHead {
            revision,
            blob_sha256: None,
            media_type: None,
            active: false,
            ..current
        })
    }

    pub(crate) fn content_changes(
        &self,
        after: u64,
        limit: u16,
    ) -> Result<Vec<ContentChange>, StoreError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT cursor, entity_kind, entity_id, revision, operation, created_at_us,
                        feed_token
             FROM content_change WHERE cursor > ?1 ORDER BY cursor LIMIT ?2",
            )
            .map_err(StoreError::storage)?;
        let rows = statement
            .query_map(
                params![u64_to_i64(after, "content cursor")?, i64::from(limit)],
                content_change_row,
            )
            .map_err(StoreError::storage)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::storage)
    }

    pub(crate) fn latest_content_cursor(&self) -> Result<u64, StoreError> {
        current_content_cursor(&self.connection)
    }

    pub(crate) fn replica_state(&self, device_id: &str) -> Result<ReplicaState, StoreError> {
        let server_id = server_id(&self.connection)?;
        let cursor = self.latest_content_cursor()?;
        let cursor_token = replica_cursor_token(&self.connection, cursor)?;
        let last_sequence = self
            .connection
            .query_row(
                "SELECT last_device_sequence FROM device
                 WHERE device_id = ?1 AND revoked_at_us IS NULL",
                [device_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(StoreError::storage)?
            .ok_or(StoreError::ReviewOwnership)?;
        let next_device_sequence = to_u64(last_sequence, "device sequence")?
            .checked_add(1)
            .ok_or(StoreError::CorruptData {
                field: "device sequence",
            })?;
        Ok(ReplicaState {
            server_id,
            cursor,
            cursor_token,
            next_device_sequence,
        })
    }

    pub(crate) fn replica_changes(
        &self,
        expected_server_id: &str,
        after: u64,
        after_token: &str,
        limit: u16,
    ) -> Result<(Vec<ReplicaChange>, u64, String), StoreError> {
        validate_replica_cursor(&self.connection, expected_server_id, after, after_token)?;
        let latest = current_content_cursor(&self.connection)?;
        let latest_token = replica_cursor_token(&self.connection, latest)?;
        let changes = self
            .content_changes(after, limit)?
            .into_iter()
            .map(|change| {
                Ok(ReplicaChange {
                    cursor: change.cursor,
                    cursor_token: change.feed_token,
                    projection: load_replica_projection(
                        &self.connection,
                        &change.entity_kind,
                        &change.entity_id,
                        change.revision,
                    )?,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok((changes, latest, latest_token))
    }

    pub(crate) fn create_replica_snapshot(
        &mut self,
        device_id: &str,
        created_at_us: u64,
        limit: u16,
    ) -> Result<SnapshotPage, StoreError> {
        let transaction = self.write_transaction()?;
        require_active_device(&transaction, device_id)?;
        let server_id = server_id(&transaction)?;
        if let Some(position) = reusable_snapshot(&transaction, device_id, &server_id)? {
            transaction.commit().map_err(StoreError::storage)?;
            return self.replica_snapshot_page(device_id, &position, limit);
        }
        let snapshot_id = Uuid::now_v7().to_string();
        let cut_cursor = current_content_cursor(&transaction)?;
        let cut_token = replica_cursor_token(&transaction, cut_cursor)?;
        transaction
            .execute(
                "DELETE FROM replica_snapshot WHERE device_id = ?1",
                [device_id],
            )
            .map_err(StoreError::storage)?;
        transaction
            .execute(
                "INSERT INTO replica_snapshot(
                     snapshot_id, device_id, server_id, cut_cursor, cut_token,
                     created_at_us
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    snapshot_id,
                    device_id,
                    server_id,
                    u64_to_i64(cut_cursor, "content cursor")?,
                    cut_token,
                    u64_to_i64(created_at_us, "snapshot timestamp")?,
                ],
            )
            .map_err(StoreError::storage)?;
        insert_snapshot_manifest(&transaction, &snapshot_id)?;
        transaction.commit().map_err(StoreError::storage)?;
        self.replica_snapshot_page(
            device_id,
            &SnapshotPosition {
                snapshot_id,
                server_id,
                cut_cursor,
                cut_token,
                after: 0,
            },
            limit,
        )
    }

    pub(crate) fn replica_snapshot_page(
        &self,
        device_id: &str,
        position: &SnapshotPosition,
        limit: u16,
    ) -> Result<SnapshotPage, StoreError> {
        validate_snapshot(&self.connection, device_id, position)?;
        let pointers = snapshot_pointers(&self.connection, position, limit)?;
        let items = pointers
            .iter()
            .map(|(_, kind, id, revision)| {
                load_replica_projection(&self.connection, kind, id, *revision)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let last = pointers
            .last()
            .map(|(ordinal, _, _, _)| *ordinal)
            .unwrap_or(position.after);
        let next = snapshot_has_more(&self.connection, &position.snapshot_id, last)?.then(|| {
            SnapshotPosition {
                after: last,
                ..position.clone()
            }
        });
        Ok(SnapshotPage {
            position: position.clone(),
            items,
            next,
        })
    }

    pub(crate) fn blob_meta(&self, digest: &str) -> Result<Option<BlobMeta>, StoreError> {
        self.connection
            .query_row(
                "SELECT content_sha256, size_bytes, media_type FROM blob
                 WHERE content_sha256 = ?1",
                [digest],
                |row| {
                    Ok(BlobMeta {
                        sha256: row.get(0)?,
                        size_bytes: row_u64(row, 1)?,
                        media_type: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::storage)
    }

    #[cfg(test)]
    pub(crate) fn expire_replica_changes_through(&mut self, cursor: u64) -> Result<(), StoreError> {
        let transaction = self.write_transaction()?;
        let latest = current_content_cursor(&transaction)?;
        let floor = transaction
            .query_row(
                "SELECT retained_after_cursor FROM sync_feed_state WHERE id = 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(StoreError::storage)?;
        if cursor < to_u64(floor, "content cursor")? || cursor > latest {
            return Err(StoreError::ReplicaCursor);
        }
        let token = replica_cursor_token(&transaction, cursor)?;
        transaction
            .execute(
                "DELETE FROM content_change WHERE cursor <= ?1",
                [u64_to_i64(cursor, "content cursor")?],
            )
            .map_err(StoreError::storage)?;
        transaction
            .execute(
                "UPDATE sync_feed_state SET retained_after_cursor = ?1,
                     retained_after_token = ?2 WHERE id = 1",
                params![u64_to_i64(cursor, "content cursor")?, token],
            )
            .map_err(StoreError::storage)?;
        transaction.commit().map_err(StoreError::storage)
    }

    pub(crate) fn backup_database(&self, path: &Path) -> Result<(), StoreError> {
        self.connection
            .backup(rusqlite::MAIN_DB, path, None)
            .map_err(StoreError::storage)
    }

    pub(crate) fn referenced_blobs(&self) -> Result<Vec<(String, u64)>, StoreError> {
        load_blob_rows(&self.connection)
    }

    pub(crate) fn document_asset_sources(&self) -> Result<Vec<DocumentAssetSource>, StoreError> {
        load_document_asset_sources(&self.connection)
    }

    pub(crate) fn rebuild_document_assets(
        &mut self,
        associations: &[DocumentAssetAssociation],
    ) -> Result<(), StoreError> {
        rebuild_document_asset_rows(&mut self.connection, associations)
    }

    pub(crate) fn verify_database_file(path: &Path) -> Result<Vec<(String, u64)>, StoreError> {
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(StoreError::storage)?;
        let version = schema_version(&connection)?;
        if version != SCHEMA_VERSION {
            return Err(StoreError::UnsupportedSchemaVersion {
                found: version,
                supported: SCHEMA_VERSION,
            });
        }
        let integrity: String = connection
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .map_err(StoreError::storage)?;
        let foreign_key_failure: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_foreign_key_check)",
                [],
                |row| row.get(0),
            )
            .map_err(StoreError::storage)?;
        if integrity != "ok" || foreign_key_failure {
            return Err(StoreError::CorruptData {
                field: "backup integrity",
            });
        }
        load_blob_rows(&connection)
    }

    pub(crate) fn settings(&self) -> Result<SettingsRecord, StoreError> {
        load_settings(&self.connection)
    }

    pub(crate) fn update_settings(
        &mut self,
        input: &SettingsInput,
        expected_revision: u64,
        updated_at_us: i64,
        author_device_id: Option<&str>,
    ) -> Result<SettingsRecord, StoreError> {
        let transaction = self.write_transaction()?;
        let current = transaction
            .query_row("SELECT revision FROM settings WHERE id = 1", [], |row| {
                row.get::<_, i64>(0)
            })
            .map_err(StoreError::storage)?;
        check_expected_number(Some(expected_revision), Some(current))?;
        let revision = next_number(Some(current))?;
        transaction
            .execute(
                "UPDATE settings SET revision = ?1, timezone = ?2,
                     day_boundary_hour = ?3, scheduler_version = ?4,
                     scheduler_json = ?5, updated_at_us = ?6 WHERE id = 1",
                params![
                    u64_to_i64(revision, "settings revision")?,
                    input.timezone,
                    i64::from(input.day_boundary_hour),
                    i64::from(input.scheduler.scheduler_version),
                    serde_json::to_string(&input.scheduler).map_err(StoreError::storage)?,
                    updated_at_us,
                ],
            )
            .map_err(StoreError::storage)?;
        transaction
            .execute("DELETE FROM tag_rule", [])
            .map_err(StoreError::storage)?;
        {
            let mut insert = transaction
                .prepare_cached("INSERT INTO tag_rule(tag, parameters_json) VALUES (?1, ?2)")
                .map_err(StoreError::storage)?;
            for (tag, rule) in &input.tag_rules {
                insert
                    .execute(params![
                        tag,
                        serde_json::to_string(rule).map_err(StoreError::storage)?
                    ])
                    .map_err(StoreError::storage)?;
            }
        }
        let settings = load_settings(&transaction)?;
        insert_settings_history(
            &transaction,
            &settings,
            "update",
            updated_at_us,
            author_device_id,
        )?;
        insert_content_change(
            &transaction,
            "settings",
            "canonical",
            revision,
            "update",
            updated_at_us,
        )?;
        transaction.commit().map_err(StoreError::storage)?;
        load_settings(&self.connection)
    }

    pub(crate) fn put_thema(
        &mut self,
        prepared: &PreparedThema,
        initial_review_day: u32,
        updated_at_us: i64,
        author_device_id: Option<&str>,
    ) -> Result<ThemaRecord, StoreError> {
        let transaction = self.write_transaction()?;
        let current = thema_revision(&transaction, prepared.thema_id.as_str())?;
        check_expected_number(prepared.expected_revision, current)?;
        validate_thema_associations(&transaction, prepared)?;
        if thema_is_unchanged(&transaction, prepared)? {
            transaction.commit().map_err(StoreError::storage)?;
            return load_thema(&self.connection, prepared.thema_id.as_str())?
                .ok_or(StoreError::NotFound);
        }
        let revision = next_number(current)?;
        upsert_thema_row(&transaction, prepared, revision, updated_at_us)?;
        replace_thema_associations(&transaction, prepared)?;
        insert_initial_review_state(&transaction, prepared, initial_review_day)?;
        let record =
            load_thema(&transaction, prepared.thema_id.as_str())?.ok_or(StoreError::NotFound)?;
        insert_thema_history(
            &transaction,
            &record,
            if current.is_some() {
                "update"
            } else {
                "create"
            },
            updated_at_us,
            author_device_id,
        )?;
        insert_initial_review_state_history(&transaction, prepared, updated_at_us)?;
        insert_content_change(
            &transaction,
            "thema",
            prepared.thema_id.as_str(),
            revision,
            if current.is_some() {
                "update"
            } else {
                "create"
            },
            updated_at_us,
        )?;
        if current.is_none() {
            insert_content_change(
                &transaction,
                "review",
                prepared.thema_id.as_str(),
                0,
                "snapshot",
                updated_at_us,
            )?;
        }
        transaction.commit().map_err(StoreError::storage)?;
        load_thema(&self.connection, prepared.thema_id.as_str())?.ok_or(StoreError::NotFound)
    }

    pub(crate) fn get_thema(&self, thema_id: &str) -> Result<Option<ThemaRecord>, StoreError> {
        load_thema(&self.connection, thema_id)
    }

    pub(crate) fn set_thema_archived(
        &mut self,
        thema_id: &str,
        expected_revision: u64,
        archived: bool,
        updated_at_us: i64,
        author_device_id: Option<&str>,
    ) -> Result<ThemaRecord, StoreError> {
        let transaction = self.write_transaction()?;
        let current = thema_revision(&transaction, thema_id)?.ok_or(StoreError::NotFound)?;
        check_expected_number(Some(expected_revision), Some(current))?;
        if !archived {
            require_thema_assets_active(&transaction, thema_id)?;
        }
        let revision = next_number(Some(current))?;
        transaction
            .execute(
                "UPDATE thema SET revision = ?2, archived_at_us = ?3, updated_at_us = ?4
             WHERE thema_id = ?1",
                params![
                    thema_id,
                    u64_to_i64(revision, "thema revision")?,
                    archived.then_some(updated_at_us),
                    updated_at_us
                ],
            )
            .map_err(StoreError::storage)?;
        let record = load_thema(&transaction, thema_id)?.ok_or(StoreError::NotFound)?;
        insert_thema_history(
            &transaction,
            &record,
            if archived { "archive" } else { "unarchive" },
            updated_at_us,
            author_device_id,
        )?;
        insert_content_change(
            &transaction,
            "thema",
            thema_id,
            revision,
            if archived { "archive" } else { "unarchive" },
            updated_at_us,
        )?;
        transaction.commit().map_err(StoreError::storage)?;
        load_thema(&self.connection, thema_id)?.ok_or(StoreError::NotFound)
    }

    pub(crate) fn search_themata(
        &self,
        query: &str,
        limit: u16,
        after: Option<&str>,
    ) -> Result<Vec<ThemaRecord>, StoreError> {
        let pattern = format!("%{}%", query.to_lowercase());
        let mut statement = self
            .connection
            .prepare(
                "SELECT DISTINCT thema.thema_id FROM thema
             LEFT JOIN thema_tag ON thema_tag.thema_id = thema.thema_id
             WHERE thema.thema_id > coalesce(?2, '')
               AND (lower(thema.keimenon) LIKE ?1 OR lower(thema_tag.tag) LIKE ?1)
             ORDER BY thema.thema_id LIMIT ?3",
            )
            .map_err(StoreError::storage)?;
        let ids = statement
            .query_map(params![pattern, after, i64::from(limit)], |row| row.get(0))
            .map_err(StoreError::storage)?
            .collect::<Result<Vec<String>, _>>()
            .map_err(StoreError::storage)?;
        ids.into_iter()
            .map(|id| load_thema(&self.connection, &id)?.ok_or(StoreError::NotFound))
            .collect()
    }

    pub(crate) fn get_node(&self, node_id: &str) -> Result<Option<NodeRecord>, StoreError> {
        load_node(&self.connection, node_id)
    }

    pub(crate) fn search_nodes(
        &self,
        query: &str,
        limit: u16,
        after: Option<&str>,
    ) -> Result<Vec<NodeRecord>, StoreError> {
        let pattern = format!("%{}%", query.to_lowercase());
        let mut statement = self
            .connection
            .prepare(
                "SELECT node_id FROM (
                     SELECT DISTINCT node.node_id FROM node
                     LEFT JOIN node_tag ON node_tag.document_id = node.document_id
                                            AND node_tag.node_id = node.node_id
                     WHERE lower(node.title) LIKE ?1 OR lower(node_tag.tag) LIKE ?1
                     UNION
                     SELECT retired.node_id FROM retired_node AS retired
                     WHERE NOT EXISTS(SELECT 1 FROM node WHERE node_id = retired.node_id)
                       AND (lower(retired.title) LIKE ?1 OR EXISTS(
                           SELECT 1 FROM json_each(retired.tags_json)
                           WHERE lower(value) LIKE ?1
                       ))
                 ) WHERE node_id > coalesce(?2, '')
                 ORDER BY node_id LIMIT ?3",
            )
            .map_err(StoreError::storage)?;
        let ids = statement
            .query_map(params![pattern, after, i64::from(limit)], |row| row.get(0))
            .map_err(StoreError::storage)?
            .collect::<Result<Vec<String>, _>>()
            .map_err(StoreError::storage)?;
        ids.into_iter()
            .map(|id| load_node(&self.connection, &id)?.ok_or(StoreError::NotFound))
            .collect()
    }

    pub(crate) fn graph_nodes(
        &self,
        node_id: &str,
        inbound: bool,
        limit: u16,
        after: Option<&str>,
    ) -> Result<Vec<NodeRecord>, StoreError> {
        let (selected, compared) = if inbound {
            ("source_id", "target_id")
        } else {
            ("target_id", "source_id")
        };
        let historical = !load_node(&self.connection, node_id)?
            .ok_or(StoreError::NotFound)?
            .active;
        let edges = if historical {
            "(SELECT source_id, target_id FROM node_edge
               UNION SELECT source_id, target_id FROM retired_node_edge)"
        } else {
            "node_edge"
        };
        let query = format!(
            "SELECT DISTINCT {selected} FROM {edges}
             WHERE {compared} = ?1 AND {selected} > coalesce(?2, '')
             ORDER BY {selected} LIMIT ?3"
        );
        let mut statement = self
            .connection
            .prepare(&query)
            .map_err(StoreError::storage)?;
        let ids = statement
            .query_map(params![node_id, after, i64::from(limit)], |row| row.get(0))
            .map_err(StoreError::storage)?
            .collect::<Result<Vec<String>, _>>()
            .map_err(StoreError::storage)?;
        ids.into_iter()
            .map(|id| load_node(&self.connection, &id)?.ok_or(StoreError::NotFound))
            .collect()
    }

    pub(crate) fn create_device(
        &mut self,
        device: &NewDevice,
        created_at_us: i64,
    ) -> Result<DeviceRecord, StoreError> {
        let scopes = stored_scopes(&device.scopes).map_err(StoreError::storage)?;
        let transaction = self.write_transaction()?;
        transaction
            .execute(
                "INSERT INTO device(
                 device_id, name, token_digest, scopes_json, created_at_us
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    device.device_id,
                    device.name,
                    device.token_digest,
                    scopes,
                    created_at_us
                ],
            )
            .map_err(StoreError::storage)?;
        insert_audit(
            &transaction,
            Some(&device.device_id),
            "device.create",
            "device",
            Some(&device.device_id),
            created_at_us,
        )?;
        transaction.commit().map_err(StoreError::storage)?;
        self.device(&device.device_id)?.ok_or(StoreError::NotFound)
    }

    pub(crate) fn devices(&self) -> Result<Vec<DeviceRecord>, StoreError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT device_id, name, scopes_json, created_at_us,
                    last_used_at_us, revoked_at_us
             FROM device ORDER BY created_at_us, device_id",
            )
            .map_err(StoreError::storage)?;
        let rows = statement
            .query_map([], device_record_row)
            .map_err(StoreError::storage)?;
        rows.map(|row| materialize_device(row.map_err(StoreError::storage)?))
            .collect()
    }

    pub(crate) fn revoke_device(
        &mut self,
        device_id: &str,
        revoked_at_us: i64,
    ) -> Result<DeviceRecord, StoreError> {
        let transaction = self.write_transaction()?;
        let changed = transaction
            .execute(
                "UPDATE device SET revoked_at_us = coalesce(revoked_at_us, ?2)
             WHERE device_id = ?1",
                params![device_id, revoked_at_us],
            )
            .map_err(StoreError::storage)?;
        if changed == 0 {
            return Err(StoreError::NotFound);
        }
        insert_audit(
            &transaction,
            Some(device_id),
            "device.revoke",
            "device",
            Some(device_id),
            revoked_at_us,
        )?;
        transaction.commit().map_err(StoreError::storage)?;
        self.device(device_id)?.ok_or(StoreError::NotFound)
    }

    pub(crate) fn authenticate(
        &mut self,
        digest: &str,
        used_at_us: i64,
    ) -> Result<Option<DevicePrincipal>, StoreError> {
        let transaction = self.write_transaction()?;
        let principal = transaction
            .query_row(
                "SELECT device_id, scopes_json FROM device
             WHERE token_digest = ?1 AND revoked_at_us IS NULL",
                [digest],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(StoreError::storage)?;
        let Some((device_id, scopes)) = principal else {
            transaction.commit().map_err(StoreError::storage)?;
            return Ok(None);
        };
        transaction
            .execute(
                "UPDATE device SET last_used_at_us = ?2 WHERE device_id = ?1",
                params![device_id, used_at_us],
            )
            .map_err(StoreError::storage)?;
        transaction.commit().map_err(StoreError::storage)?;
        Ok(Some(DevicePrincipal {
            device_id,
            scopes: parse_stored_scopes(&scopes).map_err(StoreError::storage)?,
        }))
    }

    fn device(&self, device_id: &str) -> Result<Option<DeviceRecord>, StoreError> {
        self.connection
            .query_row(
                "SELECT device_id, name, scopes_json, created_at_us,
                    last_used_at_us, revoked_at_us
             FROM device WHERE device_id = ?1",
                [device_id],
                device_record_row,
            )
            .optional()
            .map_err(StoreError::storage)?
            .map(materialize_device)
            .transpose()
    }

    fn write_transaction(&mut self) -> Result<Transaction<'_>, StoreError> {
        self.connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(StoreError::storage)
    }

    #[cfg(test)]
    fn replace(
        &mut self,
        document_id: &DocumentId,
        projection: &DocumentProjection,
        expected_revision: Option<u64>,
        fail_point: FailPoint,
    ) -> Result<StoredDocument, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(StoreError::storage)?;
        let current = load_document(&transaction, document_id)?;
        check_revision(expected_revision, current.as_ref())?;
        if current
            .as_ref()
            .is_some_and(|stored| stored.projection() == projection)
        {
            transaction.commit().map_err(StoreError::storage)?;
            return current.ok_or(StoreError::CorruptData { field: "document" });
        }
        let revision = next_revision(current.as_ref())?;
        replace_rows(&transaction, document_id, projection, revision, fail_point)?;
        transaction.commit().map_err(StoreError::storage)?;
        Ok(StoredDocument {
            document_id: document_id.clone(),
            revision,
            projection: projection.clone(),
        })
    }
}

fn replayed_device_operation(
    connection: &Connection,
    operation_id: &str,
    operation_kind: &str,
    payload_sha256: &str,
    actor_device_id: Option<&str>,
) -> Result<bool, StoreError> {
    let existing = connection
        .query_row(
            "SELECT operation_kind, payload_sha256, device_id
             FROM device_operation WHERE operation_id = ?1",
            [operation_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
        .map_err(StoreError::storage)?;
    let Some((stored_kind, stored_digest, stored_device)) = existing else {
        return Ok(false);
    };
    if actor_device_id.is_some_and(|actor| actor != stored_device) {
        return Err(StoreError::ReviewOwnership);
    }
    if stored_kind != operation_kind || stored_digest != payload_sha256 {
        return Err(StoreError::IdempotencyConflict);
    }
    Ok(true)
}

fn require_device_sequence(
    connection: &Connection,
    device_id: &str,
    sequence: u64,
) -> Result<(), StoreError> {
    let last = connection
        .query_row(
            "SELECT last_device_sequence FROM device WHERE device_id = ?1",
            [device_id],
            |row| row.get::<_, i64>(0),
        )
        .map_err(StoreError::storage)?;
    let expected =
        to_u64(last, "device sequence")?
            .checked_add(1)
            .ok_or(StoreError::CorruptData {
                field: "device sequence",
            })?;
    if sequence == expected {
        Ok(())
    } else {
        Err(StoreError::DeviceSequence { expected })
    }
}

struct DeviceOperationInsert<'a> {
    operation_id: &'a str,
    device_id: &'a str,
    device_sequence: u64,
    operation_kind: &'a str,
    format_version: u32,
    payload_sha256: &'a str,
    payload_json: &'a str,
    received_at_us: u64,
}

fn insert_device_operation(
    transaction: &Transaction<'_>,
    operation: DeviceOperationInsert<'_>,
) -> Result<(), StoreError> {
    let reused = transaction
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM device_operation
                 WHERE device_id = ?1 AND device_sequence = ?2
             )",
            params![
                operation.device_id,
                u64_to_i64(operation.device_sequence, "device sequence")?
            ],
            |row| row.get::<_, bool>(0),
        )
        .map_err(StoreError::storage)?;
    if reused {
        return Err(StoreError::IdempotencyConflict);
    }
    transaction
        .execute(
            "INSERT INTO device_operation(
                 operation_id, device_id, device_sequence, operation_kind,
                 format_version, payload_sha256, payload_json, received_at_us
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                operation.operation_id,
                operation.device_id,
                u64_to_i64(operation.device_sequence, "device sequence")?,
                operation.operation_kind,
                i64::from(operation.format_version),
                operation.payload_sha256,
                operation.payload_json,
                u64_to_i64(operation.received_at_us, "operation receipt timestamp")?,
            ],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn advance_device_sequence(
    transaction: &Transaction<'_>,
    device_id: &str,
    sequence: u64,
) -> Result<(), StoreError> {
    transaction
        .execute(
            "UPDATE device SET last_device_sequence = ?2 WHERE device_id = ?1",
            params![device_id, u64_to_i64(sequence, "device sequence")?],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn document_head_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DocumentHead> {
    Ok(DocumentHead {
        document_id: row.get(0)?,
        logical_path: row.get(1)?,
        revision: row_u64(row, 2)?,
        blob_sha256: row.get(3)?,
        active: row.get::<_, i64>(4)? != 0,
    })
}

fn document_revision_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DocumentRevision> {
    Ok(DocumentRevision {
        document_id: row.get(0)?,
        revision: row_u64(row, 1)?,
        logical_path: row.get(2)?,
        blob_sha256: row.get(3)?,
        operation: row.get(4)?,
        created_at_us: row_u64(row, 5)?,
        author_device_id: row.get(6)?,
    })
}

fn asset_head_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AssetHead> {
    Ok(AssetHead {
        logical_path: row.get(0)?,
        revision: row_u64(row, 1)?,
        blob_sha256: row.get(2)?,
        media_type: row.get(3)?,
        active: row.get::<_, i64>(4)? != 0,
    })
}

fn content_change_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ContentChange> {
    Ok(ContentChange {
        cursor: row_u64(row, 0)?,
        entity_kind: row.get(1)?,
        entity_id: row.get(2)?,
        revision: row_u64(row, 3)?,
        operation: row.get(4)?,
        created_at_us: row_u64(row, 5)?,
        feed_token: row.get(6)?,
    })
}

fn server_id(connection: &Connection) -> Result<String, StoreError> {
    connection
        .query_row(
            "SELECT server_id FROM server_identity WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .map_err(StoreError::storage)
}

fn current_content_cursor(connection: &Connection) -> Result<u64, StoreError> {
    let cursor = connection
        .query_row(
            "SELECT coalesce(max(cursor), retained_after_cursor)
             FROM sync_feed_state LEFT JOIN content_change ON true
             WHERE sync_feed_state.id = 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(StoreError::storage)?;
    to_u64(cursor, "content cursor")
}

fn validate_replica_cursor(
    connection: &Connection,
    expected_server_id: &str,
    after: u64,
    after_token: &str,
) -> Result<(), StoreError> {
    if server_id(connection)? != expected_server_id {
        return Err(StoreError::ReplicaServer);
    }
    let floor = connection
        .query_row(
            "SELECT retained_after_cursor FROM sync_feed_state WHERE id = 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(StoreError::storage)?;
    let floor = to_u64(floor, "content cursor")?;
    let latest = current_content_cursor(connection)?;
    if after < floor || after > latest || replica_cursor_token(connection, after)? != after_token {
        Err(StoreError::ReplicaCursor)
    } else {
        Ok(())
    }
}

fn replica_cursor_token(connection: &Connection, position: u64) -> Result<String, StoreError> {
    let retained = connection
        .query_row(
            "SELECT retained_after_cursor, retained_after_token
             FROM sync_feed_state WHERE id = 1",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(StoreError::storage)?;
    if position == to_u64(retained.0, "content cursor")? {
        return Ok(retained.1);
    }
    connection
        .query_row(
            "SELECT feed_token FROM content_change WHERE cursor = ?1",
            [u64_to_i64(position, "content cursor")?],
            |row| row.get(0),
        )
        .optional()
        .map_err(StoreError::storage)?
        .ok_or(StoreError::ReplicaCursor)
}

fn reusable_snapshot(
    connection: &Connection,
    device_id: &str,
    current_server_id: &str,
) -> Result<Option<SnapshotPosition>, StoreError> {
    let floor = connection
        .query_row(
            "SELECT retained_after_cursor FROM sync_feed_state WHERE id = 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(StoreError::storage)?;
    let snapshot = connection
        .query_row(
            "SELECT snapshot_id, server_id, cut_cursor, cut_token
             FROM replica_snapshot WHERE device_id = ?1",
            [device_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()
        .map_err(StoreError::storage)?;
    let Some((snapshot_id, server_id, cut_cursor, cut_token)) = snapshot else {
        return Ok(None);
    };
    let cut_cursor = to_u64(cut_cursor, "snapshot cursor")?;
    if server_id != current_server_id || cut_cursor < to_u64(floor, "content cursor")? {
        return Ok(None);
    }
    Ok(Some(SnapshotPosition {
        snapshot_id,
        server_id,
        cut_cursor,
        cut_token,
        after: 0,
    }))
}

fn require_active_device(connection: &Connection, device_id: &str) -> Result<(), StoreError> {
    let active = connection
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM device WHERE device_id = ?1 AND revoked_at_us IS NULL
             )",
            [device_id],
            |row| row.get::<_, bool>(0),
        )
        .map_err(StoreError::storage)?;
    if active {
        Ok(())
    } else {
        Err(StoreError::ReviewOwnership)
    }
}

fn insert_snapshot_manifest(
    transaction: &Transaction<'_>,
    snapshot_id: &str,
) -> Result<(), StoreError> {
    transaction
        .execute(
            "INSERT INTO replica_snapshot_item(
                 snapshot_id, ordinal, entity_kind, entity_id, revision
             )
             SELECT ?1,
                    row_number() OVER (ORDER BY sort_order, entity_id),
                    entity_kind, entity_id, revision
             FROM (
                 SELECT 1 AS sort_order, 'document' AS entity_kind,
                        document.document_id AS entity_id, document.revision
                 FROM document
                 WHERE EXISTS(
                     SELECT 1 FROM document_revision AS revision
                     WHERE revision.document_id = document.document_id
                       AND revision.revision = document.revision
                 )
                 UNION ALL
                 SELECT 2, 'asset', asset_path, revision FROM managed_asset
                 UNION ALL
                 SELECT 3, 'thema', thema_id, revision FROM thema
                 UNION ALL
                 SELECT 4, 'settings', 'canonical', revision FROM settings WHERE id = 1
                 UNION ALL
                 SELECT 5, 'review', thema_id, state_revision FROM review_state
             )",
            [snapshot_id],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn validate_snapshot(
    connection: &Connection,
    device_id: &str,
    position: &SnapshotPosition,
) -> Result<(), StoreError> {
    require_active_device(connection, device_id)?;
    let stored = connection
        .query_row(
            "SELECT snapshot.device_id, snapshot.server_id, snapshot.cut_cursor,
                    snapshot.cut_token, coalesce(max(item.ordinal), 0)
             FROM replica_snapshot AS snapshot
             LEFT JOIN replica_snapshot_item AS item
               ON item.snapshot_id = snapshot.snapshot_id
             WHERE snapshot.snapshot_id = ?1
             GROUP BY snapshot.snapshot_id",
            [&position.snapshot_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()
        .map_err(StoreError::storage)?
        .ok_or(StoreError::ReplicaCursor)?;
    let cut = to_u64(stored.2, "snapshot cursor")?;
    if stored.0 != device_id {
        return Err(StoreError::ReviewOwnership);
    }
    if stored.1 != position.server_id
        || cut != position.cut_cursor
        || stored.3 != position.cut_token
    {
        return Err(StoreError::ReplicaServer);
    }
    if position.after > to_u64(stored.4, "snapshot ordinal")? {
        return Err(StoreError::ReplicaCursor);
    }
    Ok(())
}

type SnapshotPointer = (u64, String, String, u64);

fn snapshot_pointers(
    connection: &Connection,
    position: &SnapshotPosition,
    limit: u16,
) -> Result<Vec<SnapshotPointer>, StoreError> {
    let mut statement = connection
        .prepare(
            "SELECT ordinal, entity_kind, entity_id, revision
             FROM replica_snapshot_item
             WHERE snapshot_id = ?1 AND ordinal > ?2
             ORDER BY ordinal LIMIT ?3",
        )
        .map_err(StoreError::storage)?;
    statement
        .query_map(
            params![
                position.snapshot_id,
                u64_to_i64(position.after, "snapshot ordinal")?,
                i64::from(limit),
            ],
            |row| Ok((row_u64(row, 0)?, row.get(1)?, row.get(2)?, row_u64(row, 3)?)),
        )
        .map_err(StoreError::storage)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::storage)
}

fn snapshot_has_more(
    connection: &Connection,
    snapshot_id: &str,
    after: u64,
) -> Result<bool, StoreError> {
    connection
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM replica_snapshot_item
                 WHERE snapshot_id = ?1 AND ordinal > ?2
             )",
            params![snapshot_id, u64_to_i64(after, "snapshot ordinal")?],
            |row| row.get(0),
        )
        .map_err(StoreError::storage)
}

fn load_replica_projection(
    connection: &Connection,
    kind: &str,
    id: &str,
    revision: u64,
) -> Result<ReplicaProjection, StoreError> {
    match kind {
        "document" => load_document_replica_revision(connection, id, revision)
            .map(ReplicaProjection::Document),
        "asset" => load_asset_revision(connection, id, revision).map(ReplicaProjection::Asset),
        "thema" => load_thema_revision(connection, id, revision).map(ReplicaProjection::Thema),
        "settings" => load_settings_revision(connection, revision).map(ReplicaProjection::Settings),
        "review" => {
            load_review_state_revision(connection, id, revision).map(ReplicaProjection::ReviewState)
        }
        _ => Err(StoreError::CorruptData {
            field: "content change kind",
        }),
    }
}

fn load_document_replica_revision(
    connection: &Connection,
    id: &str,
    revision: u64,
) -> Result<DocumentReplicaRevision, StoreError> {
    let document_id = DocumentId::new(id.to_owned()).map_err(|_| StoreError::CorruptData {
        field: "document ID",
    })?;
    let revision_record = connection
        .query_row(
            "SELECT document_id, revision, logical_path, blob_sha256, operation,
                    created_at_us, author_device_id
             FROM document_revision WHERE document_id = ?1 AND revision = ?2",
            params![
                document_id.as_str(),
                u64_to_i64(revision, "document revision")?
            ],
            document_revision_row,
        )
        .optional()
        .map_err(StoreError::storage)?
        .ok_or(StoreError::ReplicaCursor)?;
    let projection = connection
        .query_row(
            "SELECT projection_json FROM document_projection_revision
             WHERE document_id = ?1 AND revision = ?2",
            params![
                document_id.as_str(),
                u64_to_i64(revision, "document revision")?
            ],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(StoreError::storage)?
        .map(|json| serde_json::from_str(&json).map_err(StoreError::storage))
        .transpose()?;
    if revision_record.operation != "tombstone" && projection.is_none() {
        return Err(StoreError::CorruptData {
            field: "document projection history",
        });
    }
    Ok(DocumentReplicaRevision {
        revision: revision_record,
        projection,
    })
}

fn load_asset_revision(
    connection: &Connection,
    id: &str,
    revision: u64,
) -> Result<AssetRevision, StoreError> {
    connection
        .query_row(
            "SELECT asset.asset_path, asset.revision, asset.blob_sha256,
                    blob.size_bytes, blob.media_type, asset.operation,
                    asset.created_at_us, asset.author_device_id
             FROM asset_revision AS asset
             LEFT JOIN blob ON blob.content_sha256 = asset.blob_sha256
             WHERE asset.asset_path = ?1 AND asset.revision = ?2",
            params![id, u64_to_i64(revision, "asset revision")?],
            |row| {
                Ok(AssetRevision {
                    logical_path: row.get(0)?,
                    revision: row_u64(row, 1)?,
                    blob_sha256: row.get(2)?,
                    size_bytes: row
                        .get::<_, Option<i64>>(3)?
                        .map(u64::try_from)
                        .transpose()
                        .map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                3,
                                rusqlite::types::Type::Integer,
                                Box::new(error),
                            )
                        })?,
                    media_type: row.get(4)?,
                    operation: row.get(5)?,
                    created_at_us: row_u64(row, 6)?,
                    author_device_id: row.get(7)?,
                })
            },
        )
        .optional()
        .map_err(StoreError::storage)?
        .ok_or(StoreError::ReplicaCursor)
}

fn load_thema_revision(
    connection: &Connection,
    id: &str,
    revision: u64,
) -> Result<ThemaRevision, StoreError> {
    let row = connection
        .query_row(
            "SELECT thema_id, revision, kind, keimenon, hypothesis_json, answer_json,
                    source_guid, parathema, review_image, tags_json, node_ids_json,
                    asset_paths_json, content_sha256, archived, operation,
                    created_at_us, author_device_id
             FROM thema_revision WHERE thema_id = ?1 AND revision = ?2",
            params![id, u64_to_i64(revision, "thema revision")?],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, String>(12)?,
                    row.get::<_, bool>(13)?,
                    row.get::<_, String>(14)?,
                    row.get::<_, i64>(15)?,
                    row.get::<_, Option<String>>(16)?,
                ))
            },
        )
        .optional()
        .map_err(StoreError::storage)?
        .ok_or(StoreError::ReplicaCursor)?;
    let content = stored_thema_content(
        &row.2,
        row.3,
        serde_json::from_str(&row.4).map_err(StoreError::storage)?,
        serde_json::from_str(&row.5).map_err(StoreError::storage)?,
        row.6,
        row.7,
        row.8,
    )
    .map_err(|_| StoreError::CorruptData {
        field: "thema content history",
    })?;
    Ok(ThemaRevision {
        record: ThemaRecord {
            thema_id: row.0,
            revision: to_u64(row.1, "thema revision")?,
            content,
            tags: serde_json::from_str(&row.9).map_err(StoreError::storage)?,
            node_ids: serde_json::from_str(&row.10).map_err(StoreError::storage)?,
            asset_paths: serde_json::from_str(&row.11).map_err(StoreError::storage)?,
            content_sha256: row.12,
            archived: row.13,
        },
        operation: row.14,
        created_at_us: to_u64(row.15, "thema revision timestamp")?,
        author_device_id: row.16,
    })
}

fn load_settings_revision(
    connection: &Connection,
    revision: u64,
) -> Result<SettingsRevision, StoreError> {
    let row = connection
        .query_row(
            "SELECT revision, timezone, day_boundary_hour, scheduler_version,
                    scheduler_json, tag_rules_json, operation, created_at_us,
                    author_device_id
             FROM settings_revision WHERE revision = ?1",
            [u64_to_i64(revision, "settings revision")?],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, Option<String>>(8)?,
                ))
            },
        )
        .optional()
        .map_err(StoreError::storage)?
        .ok_or(StoreError::ReplicaCursor)?;
    let mut scheduler: crate::application::ParametersSnapshot =
        serde_json::from_str(&row.4).map_err(StoreError::storage)?;
    scheduler.scheduler_version = to_u32(row.3, "scheduler version")?;
    Ok(SettingsRevision {
        settings: SettingsRecord {
            revision: to_u64(row.0, "settings revision")?,
            timezone: row.1,
            day_boundary_hour: u8::try_from(row.2).map_err(|_| StoreError::CorruptData {
                field: "day boundary",
            })?,
            scheduler,
            tag_rules: serde_json::from_str::<BTreeMap<String, TagRule>>(&row.5)
                .map_err(StoreError::storage)?,
        },
        operation: row.6,
        created_at_us: to_u64(row.7, "settings revision timestamp")?,
        author_device_id: row.8,
    })
}

fn load_review_state_revision(
    connection: &Connection,
    id: &str,
    revision: u64,
) -> Result<ReviewStateRevision, StoreError> {
    connection
        .query_row(
            "SELECT state_json, event_id, created_at_us
             FROM review_state_revision
             WHERE thema_id = ?1 AND state_revision = ?2",
            params![id, u64_to_i64(revision, "state revision")?],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()
        .map_err(StoreError::storage)?
        .ok_or(StoreError::ReplicaCursor)
        .and_then(|(json, event_id, created_at_us)| {
            Ok(ReviewStateRevision {
                thema_id: id.to_owned(),
                state: serde_json::from_str(&json).map_err(StoreError::storage)?,
                event_id,
                created_at_us: to_u64(created_at_us, "review state timestamp")?,
            })
        })
}

fn load_settings(connection: &Connection) -> Result<SettingsRecord, StoreError> {
    let (revision, timezone, boundary, version, scheduler): (i64, String, i64, i64, String) =
        connection
            .query_row(
                "SELECT revision, timezone, day_boundary_hour, scheduler_version,
                        scheduler_json FROM settings WHERE id = 1",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .map_err(StoreError::storage)?;
    let mut scheduler: crate::application::ParametersSnapshot =
        serde_json::from_str(&scheduler).map_err(StoreError::storage)?;
    scheduler.scheduler_version = u32::try_from(version).map_err(|_| StoreError::CorruptData {
        field: "scheduler version",
    })?;
    let mut statement = connection
        .prepare("SELECT tag, parameters_json FROM tag_rule ORDER BY tag")
        .map_err(StoreError::storage)?;
    let tag_rules = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(StoreError::storage)?
        .map(|row| {
            let (tag, json) = row.map_err(StoreError::storage)?;
            let rule = serde_json::from_str::<TagRule>(&json).map_err(StoreError::storage)?;
            Ok((tag, rule))
        })
        .collect::<Result<_, StoreError>>()?;
    Ok(SettingsRecord {
        revision: u64::try_from(revision).map_err(|_| StoreError::CorruptData {
            field: "settings revision",
        })?,
        timezone,
        day_boundary_hour: u8::try_from(boundary).map_err(|_| StoreError::CorruptData {
            field: "day boundary",
        })?,
        scheduler,
        tag_rules,
    })
}

fn load_blob_rows(connection: &Connection) -> Result<Vec<(String, u64)>, StoreError> {
    let mut statement = connection
        .prepare("SELECT content_sha256, size_bytes FROM blob ORDER BY content_sha256")
        .map_err(StoreError::storage)?;
    statement
        .query_map([], |row| {
            let digest = row.get::<_, String>(0)?;
            let size = row.get::<_, i64>(1)?;
            let size = u64::try_from(size).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    1,
                    rusqlite::types::Type::Integer,
                    Box::new(error),
                )
            })?;
            Ok((digest, size))
        })
        .map_err(StoreError::storage)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::storage)
}

fn load_document_asset_sources(
    connection: &Connection,
) -> Result<Vec<DocumentAssetSource>, StoreError> {
    let mut statement = connection
        .prepare(
            "SELECT revision.document_id, revision.revision, revision.blob_sha256,
                    document.active = 1 AND document.revision = revision.revision
             FROM document_revision AS revision
             JOIN document USING(document_id)
             WHERE revision.blob_sha256 IS NOT NULL
             ORDER BY revision.document_id, revision.revision",
        )
        .map_err(StoreError::storage)?;
    statement
        .query_map([], |row| {
            Ok(DocumentAssetSource {
                document_id: row.get(0)?,
                revision: row_u64(row, 1)?,
                blob_sha256: row.get(2)?,
                active_head: row.get(3)?,
            })
        })
        .map_err(StoreError::storage)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::storage)
}

fn rebuild_document_asset_rows(
    connection: &mut Connection,
    associations: &[DocumentAssetAssociation],
) -> Result<(), StoreError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(StoreError::storage)?;
    transaction
        .execute("DELETE FROM document_asset", [])
        .map_err(StoreError::storage)?;
    transaction
        .execute("DELETE FROM document_revision_asset", [])
        .map_err(StoreError::storage)?;
    for association in associations {
        insert_document_revision_assets(
            &transaction,
            &DocumentId::new(association.document_id.clone()).map_err(|_| {
                StoreError::CorruptData {
                    field: "document ID",
                }
            })?,
            association.revision,
            &association.asset_paths,
        )?;
        if association.active_head {
            insert_current_document_assets(&transaction, association)?;
        }
    }
    transaction.commit().map_err(StoreError::storage)
}

fn insert_current_document_assets(
    transaction: &Transaction<'_>,
    association: &DocumentAssetAssociation,
) -> Result<(), StoreError> {
    let mut insert = transaction
        .prepare_cached("INSERT INTO document_asset(document_id, asset_path) VALUES (?1, ?2)")
        .map_err(StoreError::storage)?;
    for asset_path in &association.asset_paths {
        insert
            .execute(params![association.document_id, asset_path])
            .map_err(StoreError::storage)?;
    }
    Ok(())
}

fn row_u64(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value = row.get::<_, i64>(index)?;
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

fn load_document_head(
    connection: &Connection,
    document_id: &DocumentId,
) -> Result<Option<DocumentHead>, StoreError> {
    connection
        .query_row(
            "SELECT document_id, logical_path, revision, head_blob_sha256, active
             FROM document WHERE document_id = ?1",
            [document_id.as_str()],
            document_head_row,
        )
        .optional()
        .map_err(StoreError::storage)
}

fn rename_document_in(
    transaction: &Transaction<'_>,
    document_id: &DocumentId,
    logical_path: &str,
    expected_revision: u64,
    updated_at_us: i64,
    author_device_id: Option<&str>,
) -> Result<DocumentHead, StoreError> {
    let current = load_document_head(transaction, document_id)?
        .filter(|head| head.active)
        .ok_or(StoreError::NotFound)?;
    check_expected_number(Some(expected_revision), Some(current.revision as i64))?;
    if current.blob_sha256.is_none() {
        return Err(StoreError::InvalidProjection {
            reason: "document source is unavailable",
        });
    }
    let projection = load_document(transaction, document_id)?.ok_or(StoreError::NotFound)?;
    check_document_path(transaction, document_id, logical_path)?;
    let revision = next_document_revision(&current)?;
    update_document_path(
        transaction,
        document_id,
        logical_path,
        revision,
        updated_at_us,
    )?;
    record_document_rename(
        transaction,
        document_id,
        logical_path,
        revision,
        &current,
        projection.projection(),
        updated_at_us,
        author_device_id,
    )?;
    Ok(DocumentHead {
        logical_path: logical_path.to_owned(),
        revision,
        ..current
    })
}

#[allow(clippy::too_many_arguments)]
fn record_document_rename(
    transaction: &Transaction<'_>,
    document_id: &DocumentId,
    logical_path: &str,
    revision: u64,
    current: &DocumentHead,
    projection: &DocumentProjection,
    updated_at_us: i64,
    author_device_id: Option<&str>,
) -> Result<(), StoreError> {
    insert_document_history(
        transaction,
        document_id,
        logical_path,
        current.blob_sha256.as_deref(),
        revision,
        "rename",
        updated_at_us,
        author_device_id,
    )?;
    insert_document_projection_history(transaction, document_id, revision, projection)?;
    copy_document_revision_assets(transaction, document_id, revision)?;
    insert_content_change(
        transaction,
        "document",
        document_id.as_str(),
        revision,
        "rename",
        updated_at_us,
    )
}

fn tombstone_document_in(
    transaction: &Transaction<'_>,
    document_id: &DocumentId,
    expected_revision: u64,
    updated_at_us: i64,
    author_device_id: Option<&str>,
) -> Result<DocumentHead, StoreError> {
    let current = load_document_head(transaction, document_id)?
        .filter(|head| head.active)
        .ok_or(StoreError::NotFound)?;
    check_expected_number(Some(expected_revision), Some(current.revision as i64))?;
    let revision = next_document_revision(&current)?;
    retire_document(transaction, document_id, revision, updated_at_us)?;
    insert_document_history(
        transaction,
        document_id,
        &current.logical_path,
        None,
        revision,
        "tombstone",
        updated_at_us,
        author_device_id,
    )?;
    insert_content_change(
        transaction,
        "document",
        document_id.as_str(),
        revision,
        "tombstone",
        updated_at_us,
    )?;
    Ok(DocumentHead {
        revision,
        blob_sha256: None,
        active: false,
        ..current
    })
}

fn retire_document(
    transaction: &Transaction<'_>,
    document_id: &DocumentId,
    revision: u64,
    updated_at_us: i64,
) -> Result<(), StoreError> {
    transaction
        .execute(
            "UPDATE document SET active = 0, head_blob_sha256 = NULL,
                 revision = ?2, updated_at_us = ?3 WHERE document_id = ?1",
            params![
                document_id.as_str(),
                u64_to_i64(revision, "document revision")?,
                updated_at_us
            ],
        )
        .map_err(StoreError::storage)?;
    transaction
        .execute(
            "UPDATE node SET active = 0 WHERE document_id = ?1",
            [document_id.as_str()],
        )
        .map_err(StoreError::storage)?;
    transaction
        .execute(
            "DELETE FROM document_asset WHERE document_id = ?1",
            [document_id.as_str()],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn copy_document_revision_assets(
    transaction: &Transaction<'_>,
    document_id: &DocumentId,
    revision: u64,
) -> Result<(), StoreError> {
    transaction
        .execute(
            "INSERT INTO document_revision_asset(document_id, revision, asset_path)
             SELECT document_id, ?2, asset_path FROM document_asset
             WHERE document_id = ?1",
            params![
                document_id.as_str(),
                u64_to_i64(revision, "document revision")?
            ],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn next_document_revision(current: &DocumentHead) -> Result<u64, StoreError> {
    current
        .revision
        .checked_add(1)
        .ok_or(StoreError::CorruptData {
            field: "document revision",
        })
}

fn check_document_path(
    connection: &Connection,
    document_id: &DocumentId,
    logical_path: &str,
) -> Result<(), StoreError> {
    let owner = connection
        .query_row(
            "SELECT document_id FROM document WHERE path_key = ?1 AND active = 1",
            [path_key(logical_path)],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(StoreError::storage)?;
    if owner
        .as_deref()
        .is_none_or(|owner| owner == document_id.as_str())
    {
        Ok(())
    } else {
        Err(StoreError::InvalidProjection {
            reason: "logical path collides with another document",
        })
    }
}

fn update_document_path(
    transaction: &Transaction<'_>,
    document_id: &DocumentId,
    logical_path: &str,
    revision: u64,
    updated_at_us: i64,
) -> Result<(), StoreError> {
    transaction
        .execute(
            "UPDATE document SET logical_path = ?2, path_key = ?3,
                 revision = ?4, updated_at_us = ?5 WHERE document_id = ?1",
            params![
                document_id.as_str(),
                logical_path,
                path_key(logical_path),
                u64_to_i64(revision, "document revision")?,
                updated_at_us,
            ],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn insert_document_history(
    transaction: &Transaction<'_>,
    document_id: &DocumentId,
    logical_path: &str,
    blob_sha256: Option<&str>,
    revision: u64,
    operation: &str,
    created_at_us: i64,
    author_device_id: Option<&str>,
) -> Result<(), StoreError> {
    transaction
        .execute(
            "INSERT INTO document_revision(
                 document_id, revision, logical_path, blob_sha256, operation,
                 created_at_us, author_device_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                document_id.as_str(),
                u64_to_i64(revision, "document revision")?,
                logical_path,
                blob_sha256,
                operation,
                created_at_us,
                author_device_id,
            ],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn insert_document_projection_history(
    transaction: &Transaction<'_>,
    document_id: &DocumentId,
    revision: u64,
    projection: &DocumentProjection,
) -> Result<(), StoreError> {
    let snapshot = DocumentProjectionSnapshot::from(projection);
    transaction
        .execute(
            "INSERT INTO document_projection_revision(
                 document_id, revision, projection_json
             ) VALUES (?1, ?2, ?3)",
            params![
                document_id.as_str(),
                u64_to_i64(revision, "document revision")?,
                serde_json::to_string(&snapshot).map_err(StoreError::storage)?,
            ],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn load_asset_head(
    connection: &Connection,
    asset_path: &str,
) -> Result<Option<AssetHead>, StoreError> {
    connection
        .query_row(
            "SELECT asset.asset_path, asset.revision, asset.blob_sha256,
                    blob.media_type, asset.active
             FROM managed_asset AS asset
             JOIN blob ON blob.content_sha256 = asset.blob_sha256
             WHERE asset.asset_path = ?1",
            [asset_path],
            asset_head_row,
        )
        .optional()
        .map_err(StoreError::storage)
}

fn require_asset_unreferenced(connection: &Connection, asset_path: &str) -> Result<(), StoreError> {
    let unverifiable: bool = connection
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM document
                 WHERE active = 1 AND head_blob_sha256 IS NULL
             )",
            [],
            |row| row.get(0),
        )
        .map_err(StoreError::storage)?;
    if unverifiable {
        return Err(StoreError::InvalidProjection {
            reason: "active document asset references cannot be verified",
        });
    }
    let referenced: bool = connection
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM document_asset WHERE asset_path = ?1
                 UNION ALL
                 SELECT 1 FROM thema_asset AS asset
                 JOIN thema ON thema.thema_id = asset.thema_id
                 WHERE asset.asset_path = ?1 AND thema.archived_at_us IS NULL
             )",
            [asset_path],
            |row| row.get(0),
        )
        .map_err(StoreError::storage)?;
    if referenced {
        Err(StoreError::InvalidProjection {
            reason: "asset is referenced by active content",
        })
    } else {
        Ok(())
    }
}

fn thema_revision(connection: &Connection, thema_id: &str) -> Result<Option<i64>, StoreError> {
    connection
        .query_row(
            "SELECT revision FROM thema WHERE thema_id = ?1",
            [thema_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(StoreError::storage)
}

fn validate_thema_associations(
    connection: &Connection,
    prepared: &PreparedThema,
) -> Result<(), StoreError> {
    for node_id in &prepared.node_ids {
        require_active(connection, "node", "node_id", node_id.as_str())?;
    }
    for asset_path in &prepared.asset_paths {
        require_active(connection, "managed_asset", "asset_path", asset_path)?;
    }
    Ok(())
}

fn validate_note_assets(
    connection: &Connection,
    prepared: &PreparedNote,
) -> Result<(), StoreError> {
    for asset_path in &prepared.asset_paths {
        require_active(connection, "managed_asset", "asset_path", asset_path)?;
    }
    Ok(())
}

fn require_thema_assets_active(connection: &Connection, thema_id: &str) -> Result<(), StoreError> {
    let unavailable: bool = connection
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM thema_asset AS reference
                 LEFT JOIN managed_asset AS asset USING(asset_path)
                 WHERE reference.thema_id = ?1
                   AND (asset.asset_path IS NULL OR asset.active = 0)
             )",
            [thema_id],
            |row| row.get(0),
        )
        .map_err(StoreError::storage)?;
    if unavailable {
        Err(StoreError::InvalidProjection {
            reason: "thema references an inactive asset",
        })
    } else {
        Ok(())
    }
}

fn replace_document_assets(
    transaction: &Transaction<'_>,
    prepared: &PreparedNote,
) -> Result<(), StoreError> {
    transaction
        .execute(
            "DELETE FROM document_asset WHERE document_id = ?1",
            [prepared.document_id.as_str()],
        )
        .map_err(StoreError::storage)?;
    let mut insert = transaction
        .prepare_cached("INSERT INTO document_asset(document_id, asset_path) VALUES (?1, ?2)")
        .map_err(StoreError::storage)?;
    for asset_path in &prepared.asset_paths {
        insert
            .execute(params![prepared.document_id.as_str(), asset_path])
            .map_err(StoreError::storage)?;
    }
    Ok(())
}

fn insert_document_revision_assets(
    transaction: &Transaction<'_>,
    document_id: &DocumentId,
    revision: u64,
    asset_paths: &[String],
) -> Result<(), StoreError> {
    let mut insert = transaction
        .prepare_cached(
            "INSERT INTO document_revision_asset(document_id, revision, asset_path)
             VALUES (?1, ?2, ?3)",
        )
        .map_err(StoreError::storage)?;
    let revision = u64_to_i64(revision, "document revision")?;
    for asset_path in asset_paths {
        insert
            .execute(params![document_id.as_str(), revision, asset_path])
            .map_err(StoreError::storage)?;
    }
    Ok(())
}

fn require_active(
    connection: &Connection,
    table: &str,
    column: &str,
    value: &str,
) -> Result<(), StoreError> {
    let query = format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE {column} = ?1 AND active = 1)");
    let exists: bool = connection
        .query_row(&query, [value], |row| row.get(0))
        .map_err(StoreError::storage)?;
    if exists {
        Ok(())
    } else {
        Err(StoreError::NotFound)
    }
}

fn thema_is_unchanged(
    connection: &Connection,
    prepared: &PreparedThema,
) -> Result<bool, StoreError> {
    connection
        .query_row(
            "SELECT content_sha256 = ?2 AND archived_at_us IS NULL
             FROM thema WHERE thema_id = ?1",
            params![prepared.thema_id.as_str(), prepared.content_sha256],
            |row| row.get(0),
        )
        .optional()
        .map(|value| value.unwrap_or(false))
        .map_err(StoreError::storage)
}

fn upsert_thema_row(
    transaction: &Transaction<'_>,
    prepared: &PreparedThema,
    revision: u64,
    updated_at_us: i64,
) -> Result<(), StoreError> {
    let content = &prepared.content;
    transaction
        .execute(
            "INSERT INTO thema(
             thema_id, revision, kind, keimenon, hypothesis_json, answer_json,
             source_guid, parathema, review_image, content_sha256,
             created_at_us, updated_at_us
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)
         ON CONFLICT(thema_id) DO UPDATE SET
             revision = excluded.revision, kind = excluded.kind,
             keimenon = excluded.keimenon, hypothesis_json = excluded.hypothesis_json,
             answer_json = excluded.answer_json, source_guid = excluded.source_guid,
             parathema = excluded.parathema, review_image = excluded.review_image,
             content_sha256 = excluded.content_sha256, archived_at_us = NULL,
             updated_at_us = excluded.updated_at_us",
            params![
                prepared.thema_id.as_str(),
                u64_to_i64(revision, "thema revision")?,
                content.kind.as_str(),
                content.keimenon,
                serde_json::to_string(&content.hypothesis).map_err(StoreError::storage)?,
                serde_json::to_string(&content.answer).map_err(StoreError::storage)?,
                content.source_guid,
                content.parathema,
                content.review_image,
                prepared.content_sha256,
                updated_at_us,
            ],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn replace_thema_associations(
    transaction: &Transaction<'_>,
    prepared: &PreparedThema,
) -> Result<(), StoreError> {
    for table in ["thema_tag", "thema_node_link", "thema_asset"] {
        transaction
            .execute(
                &format!("DELETE FROM {table} WHERE thema_id = ?1"),
                [prepared.thema_id.as_str()],
            )
            .map_err(StoreError::storage)?;
    }
    insert_thema_tags(transaction, prepared)?;
    insert_thema_nodes(transaction, prepared)?;
    insert_thema_assets(transaction, prepared)
}

fn insert_thema_tags(
    transaction: &Transaction<'_>,
    prepared: &PreparedThema,
) -> Result<(), StoreError> {
    let mut insert = transaction
        .prepare_cached("INSERT INTO thema_tag(thema_id, tag) VALUES (?1, ?2)")
        .map_err(StoreError::storage)?;
    for tag in &prepared.tags {
        insert
            .execute(params![prepared.thema_id.as_str(), tag])
            .map_err(StoreError::storage)?;
    }
    Ok(())
}

fn insert_thema_nodes(
    transaction: &Transaction<'_>,
    prepared: &PreparedThema,
) -> Result<(), StoreError> {
    let mut insert = transaction
        .prepare_cached("INSERT INTO thema_node_link(thema_id, node_id) VALUES (?1, ?2)")
        .map_err(StoreError::storage)?;
    for node_id in &prepared.node_ids {
        insert
            .execute(params![prepared.thema_id.as_str(), node_id.as_str()])
            .map_err(StoreError::storage)?;
    }
    Ok(())
}

fn insert_thema_assets(
    transaction: &Transaction<'_>,
    prepared: &PreparedThema,
) -> Result<(), StoreError> {
    let mut insert = transaction
        .prepare_cached("INSERT INTO thema_asset(thema_id, asset_path, role) VALUES (?1, ?2, ?3)")
        .map_err(StoreError::storage)?;
    for asset_path in &prepared.asset_paths {
        let role = if prepared.content.review_image.as_deref() == Some(asset_path) {
            "review_image"
        } else {
            "content"
        };
        insert
            .execute(params![prepared.thema_id.as_str(), asset_path, role])
            .map_err(StoreError::storage)?;
    }
    Ok(())
}

fn insert_initial_review_state(
    transaction: &Transaction<'_>,
    prepared: &PreparedThema,
    initial_review_day: u32,
) -> Result<(), StoreError> {
    transaction
        .execute(
            "INSERT OR IGNORE INTO review_state VALUES (
             ?1, 0, 0, 0, 0, NULL, NULL, ?2, NULL, 0, 0, 0, 0, 0,
             0.35, 0.30, 1.3
         )",
            params![prepared.thema_id.as_str(), i64::from(initial_review_day)],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn insert_initial_review_state_history(
    transaction: &Transaction<'_>,
    prepared: &PreparedThema,
    created_at_us: i64,
) -> Result<(), StoreError> {
    let state = review::load_review_state(transaction, prepared.thema_id.as_str())?;
    let snapshot = crate::application::ReviewStateSnapshot::from(&state);
    transaction
        .execute(
            "INSERT OR IGNORE INTO review_state_revision(
                 thema_id, state_revision, state_json, event_id, created_at_us
             ) VALUES (?1, ?2, ?3, NULL, ?4)",
            params![
                prepared.thema_id.as_str(),
                u64_to_i64(snapshot.state_revision, "state revision")?,
                serde_json::to_string(&snapshot).map_err(StoreError::storage)?,
                created_at_us,
            ],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn insert_thema_history(
    transaction: &Transaction<'_>,
    record: &ThemaRecord,
    operation: &str,
    created_at_us: i64,
    author_device_id: Option<&str>,
) -> Result<(), StoreError> {
    transaction
        .execute(
            "INSERT INTO thema_revision(
                 thema_id, revision, kind, keimenon, hypothesis_json, answer_json,
                 source_guid, parathema, review_image, tags_json, node_ids_json,
                 asset_paths_json, content_sha256, archived, operation,
                 created_at_us, author_device_id
             ) VALUES (
                 ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                 ?13, ?14, ?15, ?16, ?17
             )",
            params![
                record.thema_id,
                u64_to_i64(record.revision, "thema revision")?,
                record.content.kind.as_str(),
                record.content.keimenon,
                serde_json::to_string(&record.content.hypothesis).map_err(StoreError::storage)?,
                serde_json::to_string(&record.content.answer).map_err(StoreError::storage)?,
                record.content.source_guid,
                record.content.parathema,
                record.content.review_image,
                serde_json::to_string(&record.tags).map_err(StoreError::storage)?,
                serde_json::to_string(&record.node_ids).map_err(StoreError::storage)?,
                serde_json::to_string(&record.asset_paths).map_err(StoreError::storage)?,
                record.content_sha256,
                i64::from(record.archived),
                operation,
                created_at_us,
                author_device_id,
            ],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn insert_settings_history(
    transaction: &Transaction<'_>,
    settings: &SettingsRecord,
    operation: &str,
    created_at_us: i64,
    author_device_id: Option<&str>,
) -> Result<(), StoreError> {
    transaction
        .execute(
            "INSERT INTO settings_revision(
                 revision, timezone, day_boundary_hour, scheduler_version,
                 scheduler_json, tag_rules_json, operation, created_at_us,
                 author_device_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                u64_to_i64(settings.revision, "settings revision")?,
                settings.timezone,
                i64::from(settings.day_boundary_hour),
                i64::from(settings.scheduler.scheduler_version),
                serde_json::to_string(&settings.scheduler).map_err(StoreError::storage)?,
                serde_json::to_string(&settings.tag_rules).map_err(StoreError::storage)?,
                operation,
                created_at_us,
                author_device_id,
            ],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn load_thema(connection: &Connection, thema_id: &str) -> Result<Option<ThemaRecord>, StoreError> {
    let row = connection
        .query_row(
            "SELECT thema_id, revision, kind, keimenon, hypothesis_json, answer_json,
                source_guid, parathema, review_image, content_sha256,
                archived_at_us IS NOT NULL
         FROM thema WHERE thema_id = ?1",
            [thema_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, bool>(10)?,
                ))
            },
        )
        .optional()
        .map_err(StoreError::storage)?;
    row.map(|row| materialize_thema_record(connection, row))
        .transpose()
}

type ThemaRecordRow = (
    String,
    i64,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
    bool,
);

fn materialize_thema_record(
    connection: &Connection,
    row: ThemaRecordRow,
) -> Result<ThemaRecord, StoreError> {
    let (
        id,
        revision,
        kind,
        keimenon,
        hypothesis,
        answer,
        source,
        context,
        image,
        digest,
        archived,
    ) = row;
    let content = stored_thema_content(
        &kind,
        keimenon,
        serde_json::from_str(&hypothesis).map_err(StoreError::storage)?,
        serde_json::from_str(&answer).map_err(StoreError::storage)?,
        source,
        context,
        image,
    )
    .map_err(|_| StoreError::CorruptData {
        field: "thema content",
    })?;
    Ok(ThemaRecord {
        tags: query_strings(
            connection,
            "SELECT tag FROM thema_tag WHERE thema_id = ?1 ORDER BY tag",
            &id,
        )?,
        node_ids: query_strings(
            connection,
            "SELECT node_id FROM thema_node_link WHERE thema_id = ?1 ORDER BY node_id",
            &id,
        )?,
        asset_paths: query_strings(
            connection,
            "SELECT asset_path FROM thema_asset WHERE thema_id = ?1 ORDER BY asset_path",
            &id,
        )?,
        thema_id: id,
        revision: to_u64(revision, "thema revision")?,
        content,
        content_sha256: digest,
        archived,
    })
}

fn load_node(connection: &Connection, node_id: &str) -> Result<Option<NodeRecord>, StoreError> {
    if let Some(node) = load_current_node(connection, node_id)? {
        return Ok(Some(node));
    }
    load_retired_node(connection, node_id)
}

fn load_current_node(
    connection: &Connection,
    node_id: &str,
) -> Result<Option<NodeRecord>, StoreError> {
    let row = connection
        .query_row(
            "SELECT node.node_id, node.document_id, document.logical_path, node.title,
                node.title_path_json, node.level, node.active
         FROM node JOIN document ON document.document_id = node.document_id
         WHERE node.node_id = ?1",
            [node_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, bool>(6)?,
                ))
            },
        )
        .optional()
        .map_err(StoreError::storage)?;
    row.map(|row| materialize_current_node(connection, row))
        .transpose()
}

fn materialize_current_node(
    connection: &Connection,
    row: (String, String, String, String, String, i64, bool),
) -> Result<NodeRecord, StoreError> {
    let (id, document_id, path, title, title_path, level, active) = row;
    Ok(NodeRecord {
        tags: query_node_tags(connection, &document_id, &id)?,
        thema_ids: linked_themata(connection, &id)?,
        node_id: id,
        document_id,
        logical_path: path,
        title,
        title_path: serde_json::from_str(&title_path).map_err(StoreError::storage)?,
        level: to_u32(level, "node level")?,
        active,
    })
}

fn load_retired_node(
    connection: &Connection,
    node_id: &str,
) -> Result<Option<NodeRecord>, StoreError> {
    connection
        .query_row(
            "SELECT node_id, document_id, logical_path, title, title_path_json,
                tags_json, level FROM retired_node WHERE node_id = ?1",
            [node_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            },
        )
        .optional()
        .map_err(StoreError::storage)?
        .map(|(id, document_id, path, title, title_path, tags, level)| {
            Ok(NodeRecord {
                thema_ids: linked_themata(connection, &id)?,
                node_id: id,
                document_id,
                logical_path: path,
                title,
                title_path: serde_json::from_str(&title_path).map_err(StoreError::storage)?,
                tags: serde_json::from_str(&tags).map_err(StoreError::storage)?,
                level: to_u32(level, "node level")?,
                active: false,
            })
        })
        .transpose()
}

fn query_node_tags(
    connection: &Connection,
    document_id: &str,
    node_id: &str,
) -> Result<Vec<String>, StoreError> {
    let mut statement = connection
        .prepare(
            "SELECT tag FROM node_tag WHERE document_id = ?1 AND node_id = ?2 ORDER BY ordinal",
        )
        .map_err(StoreError::storage)?;
    statement
        .query_map(params![document_id, node_id], |row| row.get(0))
        .map_err(StoreError::storage)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::storage)
}

fn linked_themata(connection: &Connection, node_id: &str) -> Result<Vec<String>, StoreError> {
    query_strings(
        connection,
        "SELECT thema_id FROM thema_node_link WHERE node_id = ?1 ORDER BY thema_id",
        node_id,
    )
}

type DeviceRow = (String, String, String, i64, Option<i64>, Option<i64>);

fn device_record_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DeviceRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
    ))
}

fn materialize_device(row: DeviceRow) -> Result<DeviceRecord, StoreError> {
    Ok(DeviceRecord {
        device_id: row.0,
        name: row.1,
        scopes: parse_stored_scopes(&row.2).map_err(StoreError::storage)?,
        created_at_us: to_u64(row.3, "device created timestamp")?,
        last_used_at_us: optional_u64(row.4, "device used timestamp")?,
        revoked_at_us: optional_u64(row.5, "device revoked timestamp")?,
    })
}

fn insert_audit(
    transaction: &Transaction<'_>,
    device_id: Option<&str>,
    action: &str,
    resource_kind: &str,
    resource_id: Option<&str>,
    created_at_us: i64,
) -> Result<(), StoreError> {
    transaction
        .execute(
            "INSERT INTO audit_event(
             device_id, action, resource_kind, resource_id, created_at_us
         ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![device_id, action, resource_kind, resource_id, created_at_us],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FailPoint {
    None,
    #[cfg(test)]
    AfterDocument,
}

fn migrate(connection: &Connection) -> Result<(), StoreError> {
    let found = schema_version(connection)?;
    if found > SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchemaVersion {
            found,
            supported: SCHEMA_VERSION,
        });
    }
    if found == 0 {
        connection
            .execute_batch(INITIAL_MIGRATION)
            .map_err(StoreError::storage)?;
    }
    if found < 2 {
        connection
            .execute_batch(SERVER_MIGRATION)
            .map_err(StoreError::storage)?;
    }
    if found < 3 {
        connection
            .execute_batch(REMOTE_REVIEW_MIGRATION)
            .map_err(StoreError::storage)?;
    }
    if found < 4 {
        connection
            .execute_batch(OFFLINE_REVIEW_MIGRATION)
            .map_err(StoreError::storage)?;
    }
    if found < 5 {
        connection
            .execute_batch(REPLICA_SYNC_MIGRATION)
            .map_err(StoreError::storage)?;
    }
    if found < 6 {
        connection
            .execute_batch(OFFLINE_CONTENT_MIGRATION)
            .map_err(StoreError::storage)?;
    }
    if found < 7 {
        connection
            .execute_batch(OFFLINE_DOCUMENT_LIFECYCLE_MIGRATION)
            .map_err(StoreError::storage)?;
    }
    if found < 8 {
        connection
            .execute_batch(DOCUMENT_ASSET_INTEGRITY_MIGRATION)
            .map_err(StoreError::storage)?;
    }
    Ok(())
}

fn backfill_current_replica_history(connection: &Connection) -> Result<(), StoreError> {
    let transaction = connection
        .unchecked_transaction()
        .map_err(StoreError::storage)?;
    backfill_projection_only_documents(&transaction)?;
    backfill_document_projections(&transaction)?;
    backfill_thema_revisions(&transaction)?;
    backfill_settings_revision(&transaction)?;
    backfill_review_state_revisions(&transaction)?;
    transaction.commit().map_err(StoreError::storage)
}

fn backfill_projection_only_documents(transaction: &Transaction<'_>) -> Result<(), StoreError> {
    let rows = {
        let mut statement = transaction
            .prepare(
                "SELECT document_id, revision, logical_path, updated_at_us
                 FROM document WHERE active = 1 AND NOT EXISTS(
                     SELECT 1 FROM document_revision AS revision
                     WHERE revision.document_id = document.document_id
                       AND revision.revision = document.revision
                 ) ORDER BY document_id",
            )
            .map_err(StoreError::storage)?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .map_err(StoreError::storage)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::storage)?
    };
    for (id, revision, stored_path, updated_at_us) in rows {
        let document_id = DocumentId::new(id).map_err(|_| StoreError::CorruptData {
            field: "document ID",
        })?;
        let logical_path = stored_path
            .filter(|path| validate_org_path(path).is_ok())
            .unwrap_or_else(|| legacy_projection_path(&document_id));
        update_projection_only_path(transaction, &document_id, &logical_path)?;
        insert_document_history(
            transaction,
            &document_id,
            &logical_path,
            None,
            to_u64(revision, "document revision")?,
            "snapshot",
            updated_at_us.max(1),
            None,
        )?;
    }
    Ok(())
}

fn legacy_projection_path(document_id: &DocumentId) -> String {
    let mut encoded = String::with_capacity(document_id.as_str().len() * 2);
    for byte in document_id.as_str().as_bytes() {
        write!(encoded, "{byte:02x}").expect("writing to a string cannot fail");
    }
    format!("legacy/{encoded}.org")
}

fn update_projection_only_path(
    transaction: &Transaction<'_>,
    document_id: &DocumentId,
    logical_path: &str,
) -> Result<(), StoreError> {
    transaction
        .execute(
            "UPDATE document SET logical_path = ?2, path_key = ?3 WHERE document_id = ?1",
            params![document_id.as_str(), logical_path, path_key(logical_path)],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn backfill_document_projections(transaction: &Transaction<'_>) -> Result<(), StoreError> {
    let ids = {
        let mut statement = transaction
            .prepare(
                "SELECT document.document_id FROM document
                 JOIN document_revision USING(document_id, revision)
                 WHERE document.active = 1 AND NOT EXISTS(
                     SELECT 1 FROM document_projection_revision AS projection
                     WHERE projection.document_id = document.document_id
                       AND projection.revision = document.revision
                 )
                 ORDER BY document.document_id",
            )
            .map_err(StoreError::storage)?;
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(StoreError::storage)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::storage)?
    };
    for id in ids {
        let document_id = DocumentId::new(id).map_err(|_| StoreError::CorruptData {
            field: "document ID",
        })?;
        let document = load_document(transaction, &document_id)?.ok_or(StoreError::NotFound)?;
        insert_document_projection_history(
            transaction,
            &document_id,
            document.revision(),
            document.projection(),
        )?;
    }
    Ok(())
}

fn backfill_thema_revisions(transaction: &Transaction<'_>) -> Result<(), StoreError> {
    let rows = {
        let mut statement = transaction
            .prepare(
                "SELECT thema.thema_id, thema.updated_at_us FROM thema
                 WHERE NOT EXISTS(
                     SELECT 1 FROM thema_revision AS revision
                     WHERE revision.thema_id = thema.thema_id
                       AND revision.revision = thema.revision
                 ) ORDER BY thema.thema_id",
            )
            .map_err(StoreError::storage)?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(StoreError::storage)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::storage)?
    };
    for (id, created_at_us) in rows {
        let record = load_thema(transaction, &id)?.ok_or(StoreError::NotFound)?;
        insert_thema_history(transaction, &record, "snapshot", created_at_us, None)?;
    }
    Ok(())
}

fn backfill_settings_revision(transaction: &Transaction<'_>) -> Result<(), StoreError> {
    let missing = transaction
        .query_row(
            "SELECT NOT EXISTS(
                 SELECT 1 FROM settings_revision AS revision
                 JOIN settings ON settings.revision = revision.revision
                 WHERE settings.id = 1
             )",
            [],
            |row| row.get::<_, bool>(0),
        )
        .map_err(StoreError::storage)?;
    if missing {
        let settings = load_settings(transaction)?;
        let created_at_us = transaction
            .query_row(
                "SELECT updated_at_us FROM settings WHERE id = 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(StoreError::storage)?;
        insert_settings_history(transaction, &settings, "snapshot", created_at_us, None)?;
    }
    Ok(())
}

fn backfill_review_state_revisions(transaction: &Transaction<'_>) -> Result<(), StoreError> {
    let ids = {
        let mut statement = transaction
            .prepare(
                "SELECT state.thema_id FROM review_state AS state
                 WHERE NOT EXISTS(
                     SELECT 1 FROM review_state_revision AS revision
                     WHERE revision.thema_id = state.thema_id
                       AND revision.state_revision = state.state_revision
                 ) ORDER BY state.thema_id",
            )
            .map_err(StoreError::storage)?;
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(StoreError::storage)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::storage)?
    };
    for id in ids {
        let state = review::load_review_state(transaction, &id)?;
        let snapshot = crate::application::ReviewStateSnapshot::from(&state);
        transaction
            .execute(
                "INSERT INTO review_state_revision(
                     thema_id, state_revision, state_json, event_id, created_at_us
                 ) VALUES (?1, ?2, ?3, NULL, ?4)",
                params![
                    id,
                    u64_to_i64(snapshot.state_revision, "state revision")?,
                    serde_json::to_string(&snapshot).map_err(StoreError::storage)?,
                    snapshot.last_reviewed_at_us.unwrap_or(0),
                ],
            )
            .map_err(StoreError::storage)?;
    }
    Ok(())
}

pub(crate) fn initialize_native_database(connection: &Connection) -> Result<(), StoreError> {
    connection
        .pragma_update(None, "foreign_keys", true)
        .map_err(StoreError::storage)?;
    migrate(connection)
}

fn schema_version(connection: &Connection) -> Result<u32, StoreError> {
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(StoreError::storage)?;
    u32::try_from(version).map_err(|_| StoreError::CorruptData {
        field: "schema version",
    })
}

fn load_legacy_database_info(
    connection: &Connection,
) -> Result<Option<LegacyDatabaseInfo>, StoreError> {
    if !table_exists(connection, "import_manifest")? {
        return Ok(None);
    }
    let source = connection
        .query_row(
            "SELECT manifest.source_schema_version,
                    (SELECT count(*) FROM import_table_count),
                    (SELECT coalesce(sum(source_rows), 0) FROM import_table_count)
             FROM import_manifest AS manifest WHERE manifest.id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(StoreError::storage)?;
    source
        .map(|(version, tables, rows)| legacy_database_info(connection, version, tables, rows))
        .transpose()
}

fn legacy_database_info(
    connection: &Connection,
    version: i64,
    tables: i64,
    rows: i64,
) -> Result<LegacyDatabaseInfo, StoreError> {
    Ok(LegacyDatabaseInfo {
        source_schema_version: to_u32(version, "legacy schema version")?,
        legacy_table_count: to_u64(tables, "legacy table count")?,
        legacy_row_count: to_u64(rows, "legacy row count")?,
        thema_count: count_rows(connection, "SELECT count(*) FROM thema")?,
        active_thema_count: count_rows(
            connection,
            "SELECT count(*) FROM thema WHERE archived_at_us IS NULL",
        )?,
        review_state_count: count_rows(connection, "SELECT count(*) FROM review_state")?,
    })
}

fn load_legacy_thema(
    connection: &Connection,
    thema_id: &ThemaId,
) -> Result<Option<LegacyThema>, StoreError> {
    let row = connection
        .query_row(LEGACY_THEMA_QUERY, [thema_id.as_str()], legacy_thema_row)
        .optional()
        .map_err(StoreError::storage)?;
    row.map(|row| materialize_legacy_thema(connection, row))
        .transpose()
}

const LEGACY_THEMA_QUERY: &str = "SELECT thema.thema_id, thema.kind, thema.keimenon, \
     thema.hypothesis_json, thema.answer_json, thema.source_guid, thema.archived_at_us, \
     thema.parathema, thema.review_image, \
     state.state_revision, state.review_count, state.legacy_review_count, \
     state.legacy_unclassified_count, state.last_reviewed_at_us, state.last_review_day, \
     state.next_review_day, state.interval_days, state.consecutive_successes, \
     state.total_successes, state.consecutive_failures, state.total_failures, \
     state.suspended, state.gnosis_plus, state.gnosis_minus, state.gnosis_synolon \
     FROM thema \
     JOIN review_state AS state ON state.thema_id = thema.thema_id \
     WHERE thema.thema_id = ?1";

struct LegacyThemaRow {
    id: String,
    kind: String,
    keimenon: String,
    hypothesis: String,
    answer: String,
    source_guid: Option<String>,
    archived_at_us: Option<i64>,
    parathema: Option<String>,
    review_image: Option<String>,
    review_state: LegacyReviewRow,
}

fn legacy_thema_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<LegacyThemaRow> {
    Ok(LegacyThemaRow {
        id: row.get(0)?,
        kind: row.get(1)?,
        keimenon: row.get(2)?,
        hypothesis: row.get(3)?,
        answer: row.get(4)?,
        source_guid: row.get(5)?,
        archived_at_us: row.get(6)?,
        parathema: row.get(7)?,
        review_image: row.get(8)?,
        review_state: LegacyReviewRow::from_row(row, 9)?,
    })
}

struct LegacyReviewRow {
    state_revision: i64,
    review_count: i64,
    legacy_review_count: i64,
    legacy_unclassified_count: i64,
    last_reviewed_at_us: Option<i64>,
    last_review_day: Option<i64>,
    next_review_day: i64,
    interval_days: Option<i64>,
    consecutive_successes: i64,
    total_successes: i64,
    consecutive_failures: i64,
    total_failures: i64,
    suspended: i64,
    gnosis_plus: f64,
    gnosis_minus: f64,
    gnosis_synolon: f64,
}

impl LegacyReviewRow {
    fn from_row(row: &rusqlite::Row<'_>, start: usize) -> rusqlite::Result<Self> {
        Ok(Self {
            state_revision: row.get(start)?,
            review_count: row.get(start + 1)?,
            legacy_review_count: row.get(start + 2)?,
            legacy_unclassified_count: row.get(start + 3)?,
            last_reviewed_at_us: row.get(start + 4)?,
            last_review_day: row.get(start + 5)?,
            next_review_day: row.get(start + 6)?,
            interval_days: row.get(start + 7)?,
            consecutive_successes: row.get(start + 8)?,
            total_successes: row.get(start + 9)?,
            consecutive_failures: row.get(start + 10)?,
            total_failures: row.get(start + 11)?,
            suspended: row.get(start + 12)?,
            gnosis_plus: row.get(start + 13)?,
            gnosis_minus: row.get(start + 14)?,
            gnosis_synolon: row.get(start + 15)?,
        })
    }
}

fn materialize_legacy_thema(
    connection: &Connection,
    row: LegacyThemaRow,
) -> Result<LegacyThema, StoreError> {
    let id = ThemaId::new(row.id).map_err(|_| StoreError::CorruptData { field: "thema ID" })?;
    Ok(LegacyThema {
        tags: load_legacy_tags(connection, &id)?,
        links: load_legacy_links(connection, &id)?,
        review_state: materialize_legacy_review_state(row.review_state)?,
        id,
        kind: row.kind,
        keimenon: row.keimenon,
        hypothesis: serde_json::from_str(&row.hypothesis).map_err(|_| StoreError::CorruptData {
            field: "thema hypothesis",
        })?,
        answer: serde_json::from_str(&row.answer).map_err(|_| StoreError::CorruptData {
            field: "thema answer",
        })?,
        source_guid: row.source_guid,
        archived_at_us: optional_u64(row.archived_at_us, "legacy archive timestamp")?,
        parathema: row.parathema,
        review_image: row.review_image,
    })
}

fn materialize_legacy_review_state(row: LegacyReviewRow) -> Result<LegacyReviewState, StoreError> {
    Ok(LegacyReviewState {
        state_revision: to_u64(row.state_revision, "legacy state revision")?,
        review_count: to_u64(row.review_count, "legacy review count")?,
        legacy_review_count: to_u64(row.legacy_review_count, "legacy imported review count")?,
        legacy_unclassified_count: to_u64(
            row.legacy_unclassified_count,
            "legacy unclassified review count",
        )?,
        last_reviewed_at_us: optional_u64(row.last_reviewed_at_us, "legacy review timestamp")?,
        last_review_day: optional_u32(row.last_review_day, "legacy last review day")?,
        next_review_day: to_u32(row.next_review_day, "legacy next review day")?,
        interval_days: optional_u32(row.interval_days, "legacy interval")?,
        consecutive_successes: to_u64(row.consecutive_successes, "legacy success streak")?,
        total_successes: to_u64(row.total_successes, "legacy total successes")?,
        consecutive_failures: to_u64(row.consecutive_failures, "legacy failure streak")?,
        total_failures: to_u64(row.total_failures, "legacy total failures")?,
        suspended: stored_bool(row.suspended, "legacy suspension")?,
        gnosis_plus: row.gnosis_plus,
        gnosis_minus: row.gnosis_minus,
        gnosis_synolon: row.gnosis_synolon,
    })
}

fn load_legacy_tags(
    connection: &Connection,
    thema_id: &ThemaId,
) -> Result<Vec<String>, StoreError> {
    query_strings(
        connection,
        "SELECT tag FROM thema_tag WHERE thema_id = ?1 ORDER BY tag",
        thema_id.as_str(),
    )
}

fn load_legacy_links(
    connection: &Connection,
    thema_id: &ThemaId,
) -> Result<Vec<ThemaId>, StoreError> {
    query_strings(
        connection,
        "SELECT node_id FROM thema_node_link
         WHERE thema_id = ?1 ORDER BY node_id",
        thema_id.as_str(),
    )?
    .into_iter()
    .map(|value| {
        ThemaId::new(value).map_err(|_| StoreError::CorruptData {
            field: "thema link",
        })
    })
    .collect()
}

fn query_strings(
    connection: &Connection,
    query: &str,
    parameter: &str,
) -> Result<Vec<String>, StoreError> {
    let mut statement = connection.prepare(query).map_err(StoreError::storage)?;
    let rows = statement
        .query_map([parameter], |row| row.get(0))
        .map_err(StoreError::storage)?;
    rows.map(|row| row.map_err(StoreError::storage)).collect()
}

fn load_due_legacy_themata(
    connection: &Connection,
    day: u32,
    limit: u16,
    after: Option<(bool, u32, i64)>,
) -> Result<Vec<DueThema>, StoreError> {
    let mut statement = connection
        .prepare(LEGACY_DUE_QUERY)
        .map_err(StoreError::storage)?;
    let (reviewed, after_day, thema_id) = after
        .map(|(reviewed, day, thema_id)| {
            (
                Some(i64::from(reviewed)),
                Some(i64::from(day)),
                Some(thema_id),
            )
        })
        .unwrap_or((None, None, None));
    let rows = statement
        .query_map(
            params![
                i64::from(day),
                reviewed,
                after_day,
                thema_id,
                i64::from(limit)
            ],
            due_thema_row,
        )
        .map_err(StoreError::storage)?;
    rows.map(|row| materialize_due_thema(row.map_err(StoreError::storage)?))
        .collect()
}

const LEGACY_DUE_QUERY: &str = "SELECT thema.thema_id, thema.kind, thema.keimenon, \
     thema.hypothesis_json, thema.answer_json, \
     state.next_review_day, state.review_count \
     FROM review_state AS state \
     JOIN thema ON thema.thema_id = state.thema_id \
     WHERE thema.archived_at_us IS NULL AND state.suspended = 0 \
     AND state.next_review_day <= ?1 \
     AND (?2 IS NULL \
          OR (state.review_count > 0) > ?2 \
          OR ((state.review_count > 0) = ?2 AND state.next_review_day > ?3) \
          OR ((state.review_count > 0) = ?2 AND state.next_review_day = ?3 \
              AND CAST(state.thema_id AS INTEGER) > ?4)) \
     ORDER BY (state.review_count > 0), state.next_review_day, \
              CAST(state.thema_id AS INTEGER) \
     LIMIT ?5";

fn due_thema_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<(String, String, String, String, String, i64, i64)> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
    ))
}

fn materialize_due_thema(
    row: (String, String, String, String, String, i64, i64),
) -> Result<DueThema, StoreError> {
    let (id, kind, keimenon, hypothesis, answer, next_review_day, review_count) = row;
    Ok(DueThema {
        id: ThemaId::new(id).map_err(|_| StoreError::CorruptData { field: "thema ID" })?,
        kind,
        keimenon,
        hypothesis: serde_json::from_str(&hypothesis).map_err(|_| StoreError::CorruptData {
            field: "thema hypothesis",
        })?,
        answer: serde_json::from_str(&answer).map_err(|_| StoreError::CorruptData {
            field: "thema answer",
        })?,
        next_review_day: to_u32(next_review_day, "legacy next review day")?,
        review_count: to_u64(review_count, "legacy review count")?,
    })
}

fn table_exists(connection: &Connection, name: &str) -> Result<bool, StoreError> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1)",
            [name],
            |row| row.get(0),
        )
        .map_err(StoreError::storage)
}

fn count_rows(connection: &Connection, query: &str) -> Result<u64, StoreError> {
    let count: i64 = connection
        .query_row(query, [], |row| row.get(0))
        .map_err(StoreError::storage)?;
    to_u64(count, "legacy row count")
}

fn stored_bool(value: i64, field: &'static str) -> Result<bool, StoreError> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(StoreError::CorruptData { field }),
    }
}

fn optional_u64(value: Option<i64>, field: &'static str) -> Result<Option<u64>, StoreError> {
    value.map(|value| to_u64(value, field)).transpose()
}

fn optional_u32(value: Option<i64>, field: &'static str) -> Result<Option<u32>, StoreError> {
    value.map(|value| to_u32(value, field)).transpose()
}

fn to_u64(value: i64, field: &'static str) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| StoreError::CorruptData { field })
}

fn to_u32(value: i64, field: &'static str) -> Result<u32, StoreError> {
    u32::try_from(value).map_err(|_| StoreError::CorruptData { field })
}

fn validate_projection(projection: &DocumentProjection) -> Result<(), StoreError> {
    let node_ids: HashSet<_> = projection.nodes().iter().map(Node::id).collect();
    if node_ids.len() != projection.nodes().len() {
        return invalid_projection("node IDs must be unique");
    }
    if projection
        .file_node_id()
        .is_some_and(|id| !node_ids.contains(id))
    {
        return invalid_projection("file node must belong to the document");
    }
    if projection
        .nodes()
        .iter()
        .filter_map(Node::parent_id)
        .any(|id| !node_ids.contains(id))
    {
        return invalid_projection("node parent must belong to the document");
    }
    if projection
        .link_occurrences()
        .iter()
        .any(|link| !node_ids.contains(link.owner()))
    {
        return invalid_projection("link owner must belong to the document");
    }
    Ok(())
}

fn invalid_projection<T>(reason: &'static str) -> Result<T, StoreError> {
    Err(StoreError::InvalidProjection { reason })
}

fn check_revision(
    expected: Option<u64>,
    current: Option<&StoredDocument>,
) -> Result<(), StoreError> {
    let actual = current.map(StoredDocument::revision);
    match expected {
        Some(expected) if actual != Some(expected) => {
            Err(StoreError::Conflict { expected, actual })
        }
        _ => Ok(()),
    }
}

fn check_expected_number(expected: Option<u64>, actual: Option<i64>) -> Result<(), StoreError> {
    if let Some(expected) = expected {
        let actual = actual.and_then(|value| u64::try_from(value).ok());
        if actual != Some(expected) {
            return Err(StoreError::Conflict { expected, actual });
        }
    }
    Ok(())
}

fn next_number(current: Option<i64>) -> Result<u64, StoreError> {
    let current = current
        .map(u64::try_from)
        .transpose()
        .map_err(|_| StoreError::CorruptData { field: "revision" })?;
    current
        .map_or(Some(1), |revision| revision.checked_add(1))
        .ok_or(StoreError::CorruptData { field: "revision" })
}

fn next_revision(current: Option<&StoredDocument>) -> Result<u64, StoreError> {
    current
        .map_or(Some(1), |stored| stored.revision().checked_add(1))
        .ok_or(StoreError::CorruptData { field: "revision" })
}

fn replace_rows(
    transaction: &Transaction<'_>,
    document_id: &DocumentId,
    projection: &DocumentProjection,
    revision: u64,
    fail_point: FailPoint,
) -> Result<(), StoreError> {
    for table in ["link_occurrence", "node_edge", "node"] {
        transaction
            .execute(
                &format!("DELETE FROM {table} WHERE document_id = ?1"),
                [document_id.as_str()],
            )
            .map_err(StoreError::storage)?;
    }
    insert_document(transaction, document_id, projection, revision)?;
    #[cfg(test)]
    if fail_point == FailPoint::AfterDocument {
        return Err(StoreError::storage(std::io::Error::other(
            "injected write failure",
        )));
    }
    let _ = fail_point;
    insert_nodes(transaction, document_id, projection.nodes())?;
    insert_occurrences(transaction, document_id, projection.link_occurrences())?;
    insert_edges(transaction, document_id, projection.semantic_edges())
}

fn insert_document(
    transaction: &Transaction<'_>,
    document_id: &DocumentId,
    projection: &DocumentProjection,
    revision: u64,
) -> Result<(), StoreError> {
    let revision =
        i64::try_from(revision).map_err(|_| StoreError::CorruptData { field: "revision" })?;
    let file_tags = serde_json::to_string(projection.file_tags()).map_err(StoreError::storage)?;
    transaction
        .execute(
            "INSERT INTO document (\
             document_id, revision, title, file_node_id, file_tags_json, \
             compatibility_sha1, content_sha256\
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(document_id) DO UPDATE SET
                 revision = excluded.revision,
                 title = excluded.title,
                 file_node_id = excluded.file_node_id,
                 file_tags_json = excluded.file_tags_json,
                 compatibility_sha1 = excluded.compatibility_sha1,
                 content_sha256 = excluded.content_sha256",
            params![
                document_id.as_str(),
                revision,
                projection.title(),
                projection.file_node_id().map(NodeId::as_str),
                file_tags,
                projection.compatibility_sha1(),
                projection.content_sha256(),
            ],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn check_path_available(
    transaction: &Transaction<'_>,
    prepared: &PreparedNote,
) -> Result<(), StoreError> {
    let owner = transaction
        .query_row(
            "SELECT document_id FROM document WHERE path_key = ?1 AND active = 1",
            [&prepared.path_key],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(StoreError::storage)?;
    if owner
        .as_deref()
        .is_none_or(|id| id == prepared.document_id.as_str())
    {
        Ok(())
    } else {
        Err(StoreError::InvalidProjection {
            reason: "logical path collides with another document",
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn replace_note_in(
    transaction: &Transaction<'_>,
    prepared: &PreparedNote,
    blob: &BlobMeta,
    expected_revision: Option<u64>,
    created_at_us: i64,
    author_device_id: Option<&str>,
    allow_noop: bool,
) -> Result<StoredDocument, StoreError> {
    let current = load_document(transaction, &prepared.document_id)?;
    check_revision(expected_revision, current.as_ref())?;
    validate_note_assets(transaction, prepared)?;
    let current_path = document_path(transaction, &prepared.document_id)?;
    require_unchanged_path(prepared, current_path.as_deref())?;
    check_path_available(transaction, prepared)?;
    if allow_noop && note_is_unchanged(transaction, prepared, current.as_ref(), &current_path)? {
        return current.ok_or(StoreError::CorruptData { field: "document" });
    }
    write_note_revision(
        transaction,
        prepared,
        blob,
        current,
        created_at_us,
        author_device_id,
    )
}

fn require_unchanged_path(
    prepared: &PreparedNote,
    current_path: Option<&str>,
) -> Result<(), StoreError> {
    if current_path.is_some_and(|path| path != prepared.logical_path) {
        Err(StoreError::PathChange)
    } else {
        Ok(())
    }
}

fn note_is_unchanged(
    transaction: &Transaction<'_>,
    prepared: &PreparedNote,
    current: Option<&StoredDocument>,
    current_path: &Option<String>,
) -> Result<bool, StoreError> {
    let head = load_document_head(transaction, &prepared.document_id)?;
    let blob_matches = head.is_some_and(|head| {
        head.active && head.blob_sha256.as_deref() == Some(prepared.blob_sha256.as_str())
    });
    Ok(blob_matches
        && current.is_some_and(|stored| {
            stored.projection() == &prepared.projection
                && current_path.as_deref() == Some(prepared.logical_path.as_str())
        }))
}

fn write_note_revision(
    transaction: &Transaction<'_>,
    prepared: &PreparedNote,
    blob: &BlobMeta,
    current: Option<StoredDocument>,
    created_at_us: i64,
    author_device_id: Option<&str>,
) -> Result<StoredDocument, StoreError> {
    let revision = next_revision(current.as_ref())?;
    retire_removed_nodes(transaction, prepared, revision)?;
    insert_blob(transaction, blob, created_at_us)?;
    replace_rows(
        transaction,
        &prepared.document_id,
        &prepared.projection,
        revision,
        FailPoint::None,
    )?;
    update_note_head(transaction, prepared, blob, revision, created_at_us)?;
    replace_document_assets(transaction, prepared)?;
    record_note_history(
        transaction,
        prepared,
        revision,
        current.is_some(),
        created_at_us,
        author_device_id,
    )?;
    Ok(StoredDocument {
        document_id: prepared.document_id.clone(),
        revision,
        projection: prepared.projection.clone(),
    })
}

fn record_note_history(
    transaction: &Transaction<'_>,
    prepared: &PreparedNote,
    revision: u64,
    existed: bool,
    created_at_us: i64,
    author_device_id: Option<&str>,
) -> Result<(), StoreError> {
    insert_document_revision(
        transaction,
        prepared,
        revision,
        existed,
        created_at_us,
        author_device_id,
    )?;
    insert_document_projection_history(
        transaction,
        &prepared.document_id,
        revision,
        &prepared.projection,
    )?;
    insert_document_revision_assets(
        transaction,
        &prepared.document_id,
        revision,
        &prepared.asset_paths,
    )?;
    insert_content_change(
        transaction,
        "document",
        prepared.document_id.as_str(),
        revision,
        if existed { "update" } else { "create" },
        created_at_us,
    )
}

fn document_path(
    connection: &Connection,
    document_id: &DocumentId,
) -> Result<Option<String>, StoreError> {
    connection
        .query_row(
            "SELECT logical_path FROM document WHERE document_id = ?1",
            [document_id.as_str()],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map(Option::flatten)
        .map_err(StoreError::storage)
}

fn insert_blob(
    transaction: &Transaction<'_>,
    blob: &BlobMeta,
    created_at_us: i64,
) -> Result<(), StoreError> {
    transaction
        .execute(
            "INSERT INTO blob(content_sha256, size_bytes, media_type, created_at_us)
             VALUES (?1, ?2, ?3, ?4) ON CONFLICT(content_sha256) DO NOTHING",
            params![
                blob.sha256,
                u64_to_i64(blob.size_bytes, "blob size")?,
                blob.media_type,
                created_at_us,
            ],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn update_note_head(
    transaction: &Transaction<'_>,
    prepared: &PreparedNote,
    blob: &BlobMeta,
    revision: u64,
    updated_at_us: i64,
) -> Result<(), StoreError> {
    transaction
        .execute(
            "UPDATE document SET logical_path = ?2, path_key = ?3,
                 head_blob_sha256 = ?4, active = 1, updated_at_us = ?5,
                 revision = ?6 WHERE document_id = ?1",
            params![
                prepared.document_id.as_str(),
                prepared.logical_path,
                prepared.path_key,
                blob.sha256,
                updated_at_us,
                u64_to_i64(revision, "document revision")?,
            ],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn insert_document_revision(
    transaction: &Transaction<'_>,
    prepared: &PreparedNote,
    revision: u64,
    existed: bool,
    created_at_us: i64,
    author_device_id: Option<&str>,
) -> Result<(), StoreError> {
    transaction
        .execute(
            "INSERT INTO document_revision(
                 document_id, revision, logical_path, blob_sha256, operation,
                 created_at_us, author_device_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                prepared.document_id.as_str(),
                u64_to_i64(revision, "document revision")?,
                prepared.logical_path,
                prepared.blob_sha256,
                if existed { "update" } else { "create" },
                created_at_us,
                author_device_id,
            ],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn insert_content_change(
    transaction: &Transaction<'_>,
    kind: &str,
    id: &str,
    revision: u64,
    operation: &str,
    created_at_us: i64,
) -> Result<(), StoreError> {
    transaction
        .execute(
            "INSERT INTO content_change(entity_kind, entity_id, revision, operation, created_at_us)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                kind,
                id,
                u64_to_i64(revision, "content revision")?,
                operation,
                created_at_us
            ],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn retire_removed_nodes(
    transaction: &Transaction<'_>,
    prepared: &PreparedNote,
    revision: u64,
) -> Result<(), StoreError> {
    let active_ids = prepared
        .projection
        .nodes()
        .iter()
        .map(|node| node.id().as_str())
        .collect::<HashSet<_>>();
    let logical_path = document_path(transaction, &prepared.document_id)?;
    let Some(logical_path) = logical_path else {
        return Ok(());
    };
    let mut statement = transaction
        .prepare(
            "SELECT node_id, title, title_path_json, level
             FROM node WHERE document_id = ?1 ORDER BY ordinal",
        )
        .map_err(StoreError::storage)?;
    let rows = statement
        .query_map([prepared.document_id.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .map_err(StoreError::storage)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::storage)?;
    drop(statement);
    for (node_id, title, title_path, level) in rows {
        if active_ids.contains(node_id.as_str()) {
            continue;
        }
        let mut tags_statement = transaction
            .prepare(
                "SELECT tag FROM node_tag
                 WHERE document_id = ?1 AND node_id = ?2 ORDER BY ordinal",
            )
            .map_err(StoreError::storage)?;
        let tags = tags_statement
            .query_map(params![prepared.document_id.as_str(), node_id], |row| {
                row.get::<_, String>(0)
            })
            .map_err(StoreError::storage)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::storage)?;
        let tags = serde_json::to_string(&tags).map_err(StoreError::storage)?;
        transaction
            .execute(
                "INSERT INTO retired_node(
                     node_id, document_id, logical_path, title, title_path_json,
                     tags_json, level, retired_at_revision
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(node_id) DO UPDATE SET
                     logical_path = excluded.logical_path,
                     title = excluded.title,
                     title_path_json = excluded.title_path_json,
                     tags_json = excluded.tags_json,
                     level = excluded.level,
                     retired_at_revision = excluded.retired_at_revision",
                params![
                    node_id,
                    prepared.document_id.as_str(),
                    logical_path,
                    title,
                    title_path,
                    tags,
                    level,
                    u64_to_i64(revision, "retired revision")?
                ],
            )
            .map_err(StoreError::storage)?;
        transaction
            .execute(
                "INSERT INTO retired_node_edge(
                     document_id, source_id, target_id, retired_at_revision
                 )
                 SELECT document_id, source_id, target_id, ?2 FROM node_edge
                 WHERE source_id = ?1 OR target_id = ?1
                 ON CONFLICT(document_id, source_id, target_id) DO UPDATE SET
                     retired_at_revision = excluded.retired_at_revision",
                params![node_id, u64_to_i64(revision, "retired revision")?],
            )
            .map_err(StoreError::storage)?;
    }
    Ok(())
}

fn insert_nodes(
    transaction: &Transaction<'_>,
    document_id: &DocumentId,
    nodes: &[Node],
) -> Result<(), StoreError> {
    for (ordinal, node) in nodes.iter().enumerate() {
        insert_node(transaction, document_id, ordinal, node)?;
        insert_node_tags(transaction, document_id, node)?;
    }
    Ok(())
}

fn insert_node(
    transaction: &Transaction<'_>,
    document_id: &DocumentId,
    ordinal: usize,
    node: &Node,
) -> Result<(), StoreError> {
    let title_path =
        serde_json::to_string(node.title_path().parts()).map_err(StoreError::storage)?;
    transaction
        .execute(
            "INSERT INTO node (\
             document_id, ordinal, node_id, title, title_path_json, parent_id, level\
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                document_id.as_str(),
                to_i64(ordinal, "node ordinal")?,
                node.id().as_str(),
                node.title(),
                title_path,
                node.parent_id().map(NodeId::as_str),
                i64::from(node.level()),
            ],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn insert_node_tags(
    transaction: &Transaction<'_>,
    document_id: &DocumentId,
    node: &Node,
) -> Result<(), StoreError> {
    for (ordinal, tag) in node.tags().iter().enumerate() {
        transaction
            .execute(
                "INSERT INTO node_tag (document_id, node_id, ordinal, tag) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    document_id.as_str(),
                    node.id().as_str(),
                    to_i64(ordinal, "tag ordinal")?,
                    tag,
                ],
            )
            .map_err(StoreError::storage)?;
    }
    Ok(())
}

fn insert_occurrences(
    transaction: &Transaction<'_>,
    document_id: &DocumentId,
    occurrences: &[LinkOccurrence],
) -> Result<(), StoreError> {
    for (ordinal, occurrence) in occurrences.iter().enumerate() {
        transaction
            .execute(
                "INSERT INTO link_occurrence (\
                 document_id, ordinal, owner_id, target_id, span_start, span_end\
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    document_id.as_str(),
                    to_i64(ordinal, "occurrence ordinal")?,
                    occurrence.owner().as_str(),
                    occurrence.target().as_str(),
                    to_i64(occurrence.span().start(), "span start")?,
                    to_i64(occurrence.span().end(), "span end")?,
                ],
            )
            .map_err(StoreError::storage)?;
    }
    Ok(())
}

fn insert_edges(
    transaction: &Transaction<'_>,
    document_id: &DocumentId,
    edges: &[NodeEdge],
) -> Result<(), StoreError> {
    for (ordinal, edge) in edges.iter().enumerate() {
        transaction
            .execute(
                "INSERT INTO node_edge (document_id, ordinal, source_id, target_id) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    document_id.as_str(),
                    to_i64(ordinal, "edge ordinal")?,
                    edge.source().as_str(),
                    edge.target().as_str(),
                ],
            )
            .map_err(StoreError::storage)?;
    }
    Ok(())
}

fn load_document(
    connection: &Connection,
    document_id: &DocumentId,
) -> Result<Option<StoredDocument>, StoreError> {
    let row = connection
        .query_row(
            "SELECT revision, title, file_node_id, file_tags_json, \
             compatibility_sha1, content_sha256 \
             FROM document WHERE document_id = ?1",
            [document_id.as_str()],
            |row| {
                Ok(DocumentRow {
                    revision: row.get(0)?,
                    title: row.get(1)?,
                    file_node_id: row.get(2)?,
                    file_tags_json: row.get(3)?,
                    compatibility_sha1: row.get(4)?,
                    content_sha256: row.get(5)?,
                })
            },
        )
        .optional()
        .map_err(StoreError::storage)?;
    row.map(|row| materialize_document(connection, document_id, row))
        .transpose()
}

struct DocumentRow {
    revision: i64,
    title: String,
    file_node_id: Option<String>,
    file_tags_json: String,
    compatibility_sha1: String,
    content_sha256: String,
}

fn materialize_document(
    connection: &Connection,
    document_id: &DocumentId,
    row: DocumentRow,
) -> Result<StoredDocument, StoreError> {
    let revision =
        u64::try_from(row.revision).map_err(|_| StoreError::CorruptData { field: "revision" })?;
    let nodes = load_nodes(connection, document_id)?;
    let link_occurrences = load_occurrences(connection, document_id)?;
    let stored_edges = load_edges(connection, document_id)?;
    let projection = DocumentProjection::new(DocumentProjectionInput {
        title: row.title,
        file_node_id: row.file_node_id.map(node_id).transpose()?,
        file_tags: from_json(&row.file_tags_json, "file tags")?,
        nodes,
        link_occurrences,
        compatibility_sha1: row.compatibility_sha1,
        content_sha256: row.content_sha256,
    })
    .map_err(|_| StoreError::CorruptData {
        field: "projection",
    })?;
    if projection.semantic_edges() != stored_edges {
        return Err(StoreError::CorruptData {
            field: "semantic edges",
        });
    }
    Ok(StoredDocument {
        document_id: document_id.clone(),
        revision,
        projection,
    })
}

fn load_nodes(connection: &Connection, document_id: &DocumentId) -> Result<Vec<Node>, StoreError> {
    let mut statement = connection
        .prepare(
            "SELECT node_id, title, title_path_json, parent_id, level \
             FROM node WHERE document_id = ?1 ORDER BY ordinal",
        )
        .map_err(StoreError::storage)?;
    let rows = statement
        .query_map([document_id.as_str()], |row| {
            Ok(NodeRow {
                id: row.get(0)?,
                title: row.get(1)?,
                title_path_json: row.get(2)?,
                parent_id: row.get(3)?,
                level: row.get(4)?,
            })
        })
        .map_err(StoreError::storage)?;
    rows.map(|row| materialize_node(connection, document_id, row.map_err(StoreError::storage)?))
        .collect()
}

struct NodeRow {
    id: String,
    title: String,
    title_path_json: String,
    parent_id: Option<String>,
    level: i64,
}

fn materialize_node(
    connection: &Connection,
    document_id: &DocumentId,
    row: NodeRow,
) -> Result<Node, StoreError> {
    let id = node_id(row.id)?;
    let title_path =
        TitlePath::new(from_json(&row.title_path_json, "title path")?).map_err(|_| {
            StoreError::CorruptData {
                field: "title path",
            }
        })?;
    let tags = load_node_tags(connection, document_id, &id)?;
    let parent = row.parent_id.map(node_id).transpose()?;
    let level = u32::try_from(row.level).map_err(|_| StoreError::CorruptData { field: "level" })?;
    Node::new(id, row.title, title_path, tags, parent, level)
        .map_err(|_| StoreError::CorruptData { field: "node" })
}

fn load_node_tags(
    connection: &Connection,
    document_id: &DocumentId,
    node_id: &NodeId,
) -> Result<Vec<String>, StoreError> {
    let mut statement = connection
        .prepare(
            "SELECT tag FROM node_tag \
             WHERE document_id = ?1 AND node_id = ?2 ORDER BY ordinal",
        )
        .map_err(StoreError::storage)?;
    let rows = statement
        .query_map(params![document_id.as_str(), node_id.as_str()], |row| {
            row.get(0)
        })
        .map_err(StoreError::storage)?;
    rows.map(|row| row.map_err(StoreError::storage)).collect()
}

fn load_occurrences(
    connection: &Connection,
    document_id: &DocumentId,
) -> Result<Vec<LinkOccurrence>, StoreError> {
    let mut statement = connection
        .prepare(
            "SELECT owner_id, target_id, span_start, span_end \
             FROM link_occurrence WHERE document_id = ?1 ORDER BY ordinal",
        )
        .map_err(StoreError::storage)?;
    let rows = statement
        .query_map([document_id.as_str()], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .map_err(StoreError::storage)?;
    rows.map(|row| {
        let (owner, target, start, end): (String, String, i64, i64) =
            row.map_err(StoreError::storage)?;
        let span =
            SourceSpan::from_offsets(to_usize(start, "span start")?, to_usize(end, "span end")?)
                .map_err(|_| StoreError::CorruptData {
                    field: "source span",
                })?;
        Ok(LinkOccurrence::new(node_id(owner)?, node_id(target)?, span))
    })
    .collect()
}

fn load_edges(
    connection: &Connection,
    document_id: &DocumentId,
) -> Result<Vec<NodeEdge>, StoreError> {
    let mut statement = connection
        .prepare(
            "SELECT source_id, target_id FROM node_edge \
             WHERE document_id = ?1 ORDER BY ordinal",
        )
        .map_err(StoreError::storage)?;
    let rows = statement
        .query_map([document_id.as_str()], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(StoreError::storage)?;
    rows.map(|row| {
        let (source, target): (String, String) = row.map_err(StoreError::storage)?;
        Ok(NodeEdge::new(node_id(source)?, node_id(target)?))
    })
    .collect()
}

fn node_id(value: String) -> Result<NodeId, StoreError> {
    NodeId::new(value).map_err(|_| StoreError::CorruptData { field: "node ID" })
}

fn from_json(value: &str, field: &'static str) -> Result<Vec<String>, StoreError> {
    serde_json::from_str(value).map_err(|_| StoreError::CorruptData { field })
}

fn to_i64(value: usize, field: &'static str) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::CorruptData { field })
}

fn u64_to_i64(value: u64, field: &'static str) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::CorruptData { field })
}

fn to_usize(value: i64, field: &'static str) -> Result<usize, StoreError> {
    usize::try_from(value).map_err(|_| StoreError::CorruptData { field })
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt};

    use sha2::Digest as _;
    use tempfile::tempdir;

    use super::{
        FailPoint, INITIAL_MIGRATION, OFFLINE_CONTENT_MIGRATION, OFFLINE_REVIEW_MIGRATION,
        REMOTE_REVIEW_MIGRATION, REPLICA_SYNC_MIGRATION, SERVER_MIGRATION, SqliteStore, StoreError,
        migrate, schema_version,
    };
    use crate::application::{
        AuthoritativeDocument, ContentBase, ContentReconciliationStatus, OfflineContentMutation,
        OfflineContentOperation, OfflineReviewOperation, ReconciliationStatus, ReplicaProjection,
        SettingsInput, generate_device, prepare_asset, prepare_note, prepare_thema, thema_content,
    };
    use crate::blob::BlobMeta;
    use crate::domain::{
        DocumentId, DocumentProjection, DocumentProjectionInput, LinkOccurrence, Node, NodeId,
        SourceSpan, TitlePath,
    };
    use crate::protocol::canonical_content_operation;

    fn projection(title: &str, digest: &str) -> DocumentProjection {
        let root = NodeId::new("root").unwrap();
        DocumentProjection::new(DocumentProjectionInput {
            title: title.into(),
            file_node_id: Some(root.clone()),
            file_tags: vec!["learning".into()],
            nodes: vec![
                Node::new(
                    root.clone(),
                    title.into(),
                    TitlePath::new(vec![title.into()]).unwrap(),
                    vec!["learning".into()],
                    None,
                    0,
                )
                .unwrap(),
            ],
            link_occurrences: vec![
                LinkOccurrence::new(
                    root.clone(),
                    NodeId::new("unresolved").unwrap(),
                    SourceSpan::from_offsets(1, 4).unwrap(),
                ),
                LinkOccurrence::new(
                    root,
                    NodeId::new("unresolved").unwrap(),
                    SourceSpan::from_offsets(8, 12).unwrap(),
                ),
            ],
            compatibility_sha1: "sha1".into(),
            content_sha256: digest.into(),
        })
        .unwrap()
    }

    fn thema(question: &str, revision: Option<u64>) -> crate::application::PreparedThema {
        prepare_thema(
            Some("thema".into()),
            thema_content(
                "basic",
                question.into(),
                vec![None],
                vec!["answer".into()],
                None,
                None,
                None,
            )
            .unwrap(),
            vec![],
            vec![],
            vec![],
            revision,
        )
        .unwrap()
    }

    fn update_timezone(store: &mut SqliteStore, revision: u64, timestamp: i64) {
        let settings = store.settings().unwrap();
        store
            .update_settings(
                &SettingsInput {
                    timezone: "UTC".into(),
                    day_boundary_hour: settings.day_boundary_hour,
                    scheduler: settings.scheduler,
                    tag_rules: settings.tag_rules,
                },
                revision,
                timestamp,
                None,
            )
            .unwrap();
    }

    #[test]
    fn foreign_keys_are_enabled() {
        let store = SqliteStore::open_in_memory().unwrap();
        let enabled: bool = store
            .connection
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .unwrap();
        assert!(enabled);
    }

    #[test]
    fn foreign_keys_reject_orphaned_rows() {
        let store = SqliteStore::open_in_memory().unwrap();
        let error = store
            .connection
            .execute(
                "INSERT INTO node_tag (document_id, node_id, ordinal, tag) \
                 VALUES ('missing-document', 'missing-node', 0, 'tag')",
                [],
            )
            .unwrap_err();
        assert!(error.to_string().contains("FOREIGN KEY constraint failed"));
    }

    #[test]
    fn replacement_preserves_thema_associations() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let id = DocumentId::new("document").unwrap();
        store
            .replace_document(&id, &projection("Original", "digest-one"), None)
            .unwrap();
        store
            .connection
            .execute("INSERT INTO thema (thema_id) VALUES ('thema')", [])
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO thema_concept (thema_id, concept_id, relation) \
                 VALUES ('thema', 'root', 'primary')",
                [],
            )
            .unwrap();

        store
            .replace_document(&id, &projection("Replacement", "digest-two"), Some(1))
            .unwrap();

        let count: u32 = store
            .connection
            .query_row("SELECT COUNT(*) FROM thema_concept", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn forced_mid_write_failure_rolls_back_previous_projection() {
        let directory = tempdir().unwrap();
        let mut store = SqliteStore::open(directory.path().join("gnosis.db")).unwrap();
        let id = DocumentId::new("document").unwrap();
        let original = projection("Original", "digest-one");
        store.replace_document(&id, &original, None).unwrap();

        let error = store
            .replace(
                &id,
                &projection("Replacement", "digest-two"),
                Some(1),
                FailPoint::AfterDocument,
            )
            .unwrap_err();
        assert!(matches!(error, StoreError::Storage(_)));

        let stored = store.get_document(&id).unwrap().unwrap();
        assert_eq!(stored.revision(), 1);
        assert_eq!(stored.projection(), &original);
    }

    #[test]
    fn migrates_and_reopens_database() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("gnosis.db");
        assert_eq!(
            SqliteStore::open(&path).unwrap().schema_version().unwrap(),
            8
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            SqliteStore::open(&path).unwrap().schema_version().unwrap(),
            8
        );
    }

    #[test]
    fn upgrades_an_existing_schema_v2_database() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection.execute_batch(INITIAL_MIGRATION).unwrap();
        connection.execute_batch(SERVER_MIGRATION).unwrap();
        assert_eq!(schema_version(&connection).unwrap(), 2);

        migrate(&connection).unwrap();

        assert_eq!(schema_version(&connection).unwrap(), 8);
        for (table, column) in [
            ("retired_node_edge", "source_id"),
            ("review_attempt", "thema_revision"),
            ("review_attempt", "parameters_json"),
            ("review_attempt", "fuzz_sample"),
            ("review_session", "owner_device_id"),
            ("review_session", "review_mode"),
            ("device", "last_device_sequence"),
            ("server_identity", "server_id"),
            ("replica_snapshot_item", "entity_kind"),
            ("thema_revision", "content_sha256"),
            ("settings_revision", "tag_rules_json"),
            ("review_state_revision", "state_json"),
        ] {
            let present: bool = connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
                    [table, column],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(present, "missing {table}.{column}");
        }
    }

    #[test]
    fn upgrades_projection_only_unix_documents_into_snapshot_history() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("projection-only.sqlite");
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection.execute_batch(INITIAL_MIGRATION).unwrap();
        connection.execute_batch(SERVER_MIGRATION).unwrap();
        connection.execute_batch(REMOTE_REVIEW_MIGRATION).unwrap();
        connection.execute_batch(OFFLINE_REVIEW_MIGRATION).unwrap();
        let mut old_store = SqliteStore { connection };
        let document_id = DocumentId::new("../legacy/socket").unwrap();
        old_store
            .replace_document(&document_id, &projection("Legacy", "digest"), None)
            .unwrap();
        drop(old_store);

        let mut store = SqliteStore::open(&path).unwrap();
        let device = generate_device("Replica".into(), vec!["read".into()]).unwrap();
        let device_id = device.device_id.clone();
        store.create_device(&device, 1).unwrap();
        let snapshot = store.create_replica_snapshot(&device_id, 2, 10).unwrap();
        let document = snapshot
            .items
            .iter()
            .find_map(|projection| match projection {
                ReplicaProjection::Document(document) => Some(document),
                _ => None,
            })
            .unwrap();

        assert_eq!(document.revision.operation, "snapshot");
        assert!(document.revision.blob_sha256.is_none());
        assert!(document.revision.logical_path.starts_with("legacy/"));
        assert_eq!(document.projection.as_ref().unwrap().title, "Legacy");
    }

    #[test]
    fn upgrades_populated_schema_v3_without_losing_rows_or_cursor_sequence() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection
            .execute_batch("PRAGMA foreign_keys = ON;")
            .unwrap();
        connection.execute_batch(INITIAL_MIGRATION).unwrap();
        connection.execute_batch(SERVER_MIGRATION).unwrap();
        connection.execute_batch(REMOTE_REVIEW_MIGRATION).unwrap();
        connection
            .execute(
                "INSERT INTO device(
                     device_id, name, token_digest, scopes_json, created_at_us
                 ) VALUES ('device', 'Device', ?1, '[\"review\"]', 1)",
                ["a".repeat(64)],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO review_session(
                     session_id, source_kind, source_json, timezone, day_boundary_hour,
                     scheduler_json, status, started_at_us
                 ) VALUES (
                     'session', 'manual', '{\"kind\":\"manual\",\"manual_ids\":[\"thema\"]}',
                     'UTC', 0, '{}', 'active', 1
                 )",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO content_change(
                     entity_kind, entity_id, revision, operation, created_at_us
                 ) VALUES ('settings', 'settings', 1, 'update', 1)",
                [],
            )
            .unwrap();
        let old_cursor: i64 = connection
            .query_row("SELECT max(cursor) FROM content_change", [], |row| {
                row.get(0)
            })
            .unwrap();

        migrate(&connection).unwrap();

        let device: (i64, i64) = connection
            .query_row(
                "SELECT count(*), last_device_sequence FROM device WHERE device_id = 'device'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(device, (1, 0));
        let session: (Option<String>, String) = connection
            .query_row(
                "SELECT owner_device_id, review_mode FROM review_session
                 WHERE session_id = 'session'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(session, (None, "online".into()));
        connection
            .execute(
                "INSERT INTO content_change(
                     entity_kind, entity_id, revision, operation, created_at_us
                 ) VALUES ('review', 'thema', 1, 'commit', 2)",
                [],
            )
            .unwrap();
        let new_cursor: i64 = connection
            .query_row("SELECT max(cursor) FROM content_change", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(new_cursor, old_cursor + 1);
        let violations: i64 = connection
            .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(violations, 0);
    }

    #[test]
    fn upgrades_populated_schema_v4_with_replica_history_and_floor() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection
            .execute_batch("PRAGMA foreign_keys = ON;")
            .unwrap();
        connection.execute_batch(INITIAL_MIGRATION).unwrap();
        connection.execute_batch(SERVER_MIGRATION).unwrap();
        connection.execute_batch(REMOTE_REVIEW_MIGRATION).unwrap();
        connection.execute_batch(OFFLINE_REVIEW_MIGRATION).unwrap();
        connection
            .execute(
                "INSERT INTO thema(
                     thema_id, revision, kind, keimenon, hypothesis_json,
                     answer_json, content_sha256, created_at_us, updated_at_us
                 ) VALUES (?1, 1, 'basic', 'Question', '[null]', '[\"answer\"]', ?2, 1, 1)",
                ("thema", "a".repeat(64)),
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO review_state VALUES (
                     'thema', 0, 0, 0, 0, NULL, NULL, 20260715, NULL,
                     0, 0, 0, 0, 0, 0.35, 0.30, 1.3
                 )",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO content_change(
                     entity_kind, entity_id, revision, operation, created_at_us
                 ) VALUES ('thema', 'thema', 1, 'create', 1)",
                [],
            )
            .unwrap();
        let old_cursor: i64 = connection
            .query_row("SELECT max(cursor) FROM content_change", [], |row| {
                row.get(0)
            })
            .unwrap();

        migrate(&connection).unwrap();

        assert_eq!(schema_version(&connection).unwrap(), 8);
        let floor: i64 = connection
            .query_row(
                "SELECT retained_after_cursor FROM sync_feed_state",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(floor, old_cursor);
        for table in [
            "thema_revision",
            "settings_revision",
            "review_state_revision",
        ] {
            let count: i64 = connection
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 1, "missing {table} backfill");
        }
        let server_id: String = connection
            .query_row("SELECT server_id FROM server_identity", [], |row| {
                row.get(0)
            })
            .unwrap();
        migrate(&connection).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT server_id FROM server_identity", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
            server_id
        );
    }

    #[test]
    fn upgrades_schema_v5_review_journal_into_the_shared_device_ledger() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/protocol/offline-review-v1.json"
        ))
        .unwrap();
        let operation: OfflineReviewOperation =
            serde_json::from_value(fixture["submission"]["operation"].clone()).unwrap();
        let canonical_payload = serde_json::to_string(&fixture["submission"]).unwrap();
        let payload_sha256 = format!("{:x}", sha2::Sha256::digest(canonical_payload.as_bytes()));
        let state_json =
            serde_json::to_string(&fixture["result"]["result"]["authoritative_state"]).unwrap();
        let session_json = serde_json::to_string(&fixture["result"]["result"]["session"]).unwrap();
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection
            .execute_batch("PRAGMA foreign_keys = ON;")
            .unwrap();
        for migration in [
            INITIAL_MIGRATION,
            SERVER_MIGRATION,
            REMOTE_REVIEW_MIGRATION,
            OFFLINE_REVIEW_MIGRATION,
            REPLICA_SYNC_MIGRATION,
        ] {
            connection.execute_batch(migration).unwrap();
        }
        connection
            .execute(
                "INSERT INTO device(
                     device_id, name, token_digest, scopes_json, created_at_us
                 ) VALUES ('device', 'Device', ?1, '[\"review\"]', 1)",
                ["a".repeat(64)],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO review_session(
                     session_id, source_kind, source_json, timezone, day_boundary_hour,
                     scheduler_json, status, started_at_us, owner_device_id, review_mode
                 ) VALUES (
                     'session', 'manual', '{}', 'UTC', 0, '{}', 'active', 1,
                     'device', 'offline'
                 )",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO review_pack(
                     pack_id, format_version, session_id, device_id, lease_id,
                     status, cursor, issued_at_us, cursor_server_id, cursor_token
                 ) SELECT 'pack', 1, 'session', 'device', 'lease', 'active', 0, 1,
                          server_identity.server_id, sync_feed_state.retained_after_token
                   FROM server_identity JOIN sync_feed_state ON sync_feed_state.id = 1",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO offline_review_operation(
                     attempt_id, pack_id, device_id, device_sequence, format_version,
                     payload_sha256, payload_json, received_at_us
                 ) VALUES ('attempt', 'pack', 'device', 1, 1, ?1, ?2, 2)",
                rusqlite::params![payload_sha256, canonical_payload],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO offline_review_result(
                     attempt_id, format_version, status, conflict_reason, event_id,
                     authoritative_state_json, session_json, cursor, reconciled_at_us,
                     cursor_server_id, cursor_token
                 ) SELECT 'attempt', 1, 'conflict', 'scheduling_changed', NULL,
                          ?1, ?2, 0, 3, server_identity.server_id,
                          sync_feed_state.retained_after_token
                   FROM server_identity JOIN sync_feed_state ON sync_feed_state.id = 1",
                rusqlite::params![state_json, session_json],
            )
            .unwrap();

        migrate(&connection).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT last_device_sequence FROM device WHERE device_id = 'device'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        let mut store = SqliteStore { connection };
        let replay = store
            .reconcile_offline_operation(1, &operation, &canonical_payload, Some("device"), 4)
            .unwrap();
        assert_eq!(replay.status, ReconciliationStatus::Conflict);
        assert_eq!(replay.attempt_id, "attempt");
    }

    #[test]
    fn upgrades_populated_schema_v6_content_ledger_without_changing_replay() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection
            .execute_batch("PRAGMA foreign_keys = ON;")
            .unwrap();
        for migration in [
            INITIAL_MIGRATION,
            SERVER_MIGRATION,
            REMOTE_REVIEW_MIGRATION,
            OFFLINE_REVIEW_MIGRATION,
            REPLICA_SYNC_MIGRATION,
            OFFLINE_CONTENT_MIGRATION,
        ] {
            connection.execute_batch(migration).unwrap();
        }
        let mut store = SqliteStore { connection };
        let device = generate_device("Offline".into(), vec!["content:write".into()]).unwrap();
        let device_id = device.device_id.clone();
        store.create_device(&device, 1).unwrap();
        let state = store.replica_state(&device_id).unwrap();
        let org = "#+title: Preserved\n";
        let content_sha256 = format!("{:x}", sha2::Sha256::digest(org.as_bytes()));
        let operation = OfflineContentOperation {
            operation_id: "operation".into(),
            device_id: device_id.clone(),
            device_sequence: 1,
            cursor_server_id: state.server_id.clone(),
            cursor: state.cursor,
            cursor_token: state.cursor_token.clone(),
            authored_at_us: 2,
            mutation: OfflineContentMutation::Put {
                document_id: "document".into(),
                logical_path: "note.org".into(),
                content_sha256: content_sha256.clone(),
                org: org.into(),
                base: ContentBase::Absent,
            },
        };
        let payload = canonical_content_operation(&operation).unwrap();
        let payload_sha256 = format!("{:x}", sha2::Sha256::digest(payload.as_bytes()));
        let authoritative = serde_json::to_string(&AuthoritativeDocument {
            document_id: "document".into(),
            logical_path: "note.org".into(),
            revision: 1,
            content_sha256: Some(content_sha256.clone()),
            active: true,
        })
        .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO device_operation VALUES (?1, ?2, 1, 'content', 1, ?3, ?4, 3)",
                rusqlite::params![operation.operation_id, device_id, payload_sha256, payload],
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO offline_content_operation VALUES (
                     ?1, 'document', ?2, 'put_document', 'absent', NULL, NULL, NULL,
                     ?3, ?4, ?5, 2
                 )",
                rusqlite::params![
                    operation.operation_id,
                    content_sha256,
                    state.server_id,
                    i64::try_from(state.cursor).unwrap(),
                    state.cursor_token,
                ],
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO offline_content_result VALUES (
                     ?1, 1, 'accepted', NULL, ?2, ?3, ?4, ?5, 4
                 )",
                rusqlite::params![
                    operation.operation_id,
                    authoritative,
                    i64::try_from(state.cursor).unwrap(),
                    state.server_id,
                    state.cursor_token,
                ],
            )
            .unwrap();
        let chained_org = "#+title: Chained\n";
        let chained_digest = format!("{:x}", sha2::Sha256::digest(chained_org.as_bytes()));
        let chained = OfflineContentOperation {
            operation_id: "chained".into(),
            device_id: device_id.clone(),
            device_sequence: 2,
            cursor_server_id: state.server_id.clone(),
            cursor: state.cursor,
            cursor_token: state.cursor_token.clone(),
            authored_at_us: 3,
            mutation: OfflineContentMutation::Put {
                document_id: "document".into(),
                logical_path: "note.org".into(),
                content_sha256: chained_digest.clone(),
                org: chained_org.into(),
                base: ContentBase::Revision {
                    revision: 1,
                    content_sha256: Some(content_sha256),
                    predecessor_operation_id: Some(operation.operation_id.clone()),
                },
            },
        };
        let chained_payload = canonical_content_operation(&chained).unwrap();
        let chained_payload_sha256 =
            format!("{:x}", sha2::Sha256::digest(chained_payload.as_bytes()));
        store
            .connection
            .execute(
                "INSERT INTO device_operation VALUES (?1, ?2, 2, 'content', 1, ?3, ?4, 4)",
                rusqlite::params![
                    chained.operation_id,
                    device_id,
                    chained_payload_sha256,
                    chained_payload,
                ],
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO offline_content_operation VALUES (
                     ?1, 'document', ?2, 'put_document', 'revision', 1, ?3, ?4,
                     ?5, ?6, ?7, 3
                 )",
                rusqlite::params![
                    chained.operation_id,
                    chained_digest,
                    chained.base().content_sha256(),
                    operation.operation_id,
                    state.server_id,
                    i64::try_from(state.cursor).unwrap(),
                    state.cursor_token,
                ],
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO offline_content_result VALUES (
                     ?1, 1, 'conflict', 'base_changed', ?2, ?3, ?4, ?5, 5
                 )",
                rusqlite::params![
                    chained.operation_id,
                    authoritative,
                    i64::try_from(state.cursor).unwrap(),
                    state.server_id,
                    state.cursor_token,
                ],
            )
            .unwrap();

        migrate(&store.connection).unwrap();
        assert_eq!(schema_version(&store.connection).unwrap(), 8);
        let violations: i64 = store
            .connection
            .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(violations, 0);
        let replay = store
            .reconcile_content_operation(
                1,
                &operation,
                &canonical_content_operation(&operation).unwrap(),
                Some(&device_id),
                5,
                Ok((None, None)),
            )
            .unwrap();
        assert_eq!(replay.status, ContentReconciliationStatus::Accepted);
        assert_eq!(replay.authoritative.unwrap().revision, 1);
        let replay = store
            .reconcile_content_operation(
                1,
                &chained,
                &canonical_content_operation(&chained).unwrap(),
                Some(&device_id),
                6,
                Ok((None, None)),
            )
            .unwrap();
        assert_eq!(replay.status, ContentReconciliationStatus::Conflict);
    }

    #[test]
    fn replica_cursors_reject_other_servers_rollback_and_expiration() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let device = generate_device("Replica".into(), vec!["read".into()]).unwrap();
        let device_id = device.device_id.clone();
        store.create_device(&device, 1).unwrap();
        let initial = store.replica_state(&device_id).unwrap();
        update_timezone(&mut store, 1, 2);
        let state = store.replica_state(&device_id).unwrap();
        assert_eq!(state.cursor, 1);

        assert!(matches!(
            store.replica_changes("other-server", 1, &state.cursor_token, 10),
            Err(StoreError::ReplicaServer)
        ));
        assert!(matches!(
            store.replica_changes(&state.server_id, 2, &state.cursor_token, 10),
            Err(StoreError::ReplicaCursor)
        ));

        store.expire_replica_changes_through(1).unwrap();
        assert!(matches!(
            store.replica_changes(&state.server_id, 0, &initial.cursor_token, 10),
            Err(StoreError::ReplicaCursor)
        ));
        assert_eq!(
            store
                .replica_changes(&state.server_id, 1, &state.cursor_token, 10)
                .unwrap(),
            (vec![], 1, state.cursor_token)
        );
    }

    #[test]
    fn replica_cursor_token_rejects_a_reused_position_after_restore() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let device = generate_device("Replica".into(), vec!["read".into()]).unwrap();
        let device_id = device.device_id.clone();
        store.create_device(&device, 1).unwrap();
        update_timezone(&mut store, 1, 2);
        update_timezone(&mut store, 2, 3);
        let abandoned_branch = store.replica_state(&device_id).unwrap();
        assert_eq!(abandoned_branch.cursor, 2);

        store
            .connection
            .execute("DELETE FROM content_change WHERE cursor = 2", [])
            .unwrap();
        store
            .connection
            .execute(
                "UPDATE sqlite_sequence SET seq = 1 WHERE name = 'content_change'",
                [],
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO content_change(
                     entity_kind, entity_id, revision, operation, created_at_us
                 ) VALUES ('settings', 'canonical', 2, 'update', 4)",
                [],
            )
            .unwrap();
        let restored_branch = store.replica_state(&device_id).unwrap();
        assert_eq!(restored_branch.cursor, abandoned_branch.cursor);
        assert_ne!(restored_branch.cursor_token, abandoned_branch.cursor_token);

        assert!(matches!(
            store.replica_changes(
                &abandoned_branch.server_id,
                abandoned_branch.cursor,
                &abandoned_branch.cursor_token,
                10,
            ),
            Err(StoreError::ReplicaCursor)
        ));
    }

    #[test]
    fn snapshot_manifest_is_fixed_and_delta_resolves_exact_revision() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let owner = generate_device("Owner".into(), vec!["read".into()]).unwrap();
        let other = generate_device("Other".into(), vec!["read".into()]).unwrap();
        let owner_id = owner.device_id.clone();
        store.create_device(&owner, 1).unwrap();
        store.create_device(&other, 2).unwrap();
        store
            .put_thema(&thema("Question", None), 20260715, 3, None)
            .unwrap();

        let first = store.create_replica_snapshot(&owner_id, 4, 1).unwrap();
        let retry = store.create_replica_snapshot(&owner_id, 5, 1).unwrap();
        assert_eq!(retry, first);
        let cut = first.position.cut_cursor;
        assert!(matches!(first.items[0], ReplicaProjection::Thema(_)));
        let mut ahead = first.position.clone();
        ahead.after = u64::MAX;
        assert!(matches!(
            store.replica_snapshot_page(&owner_id, &ahead, 1),
            Err(StoreError::ReplicaCursor)
        ));
        update_timezone(&mut store, 1, 6);

        let mut position = first.next.unwrap();
        let mut remaining = Vec::new();
        loop {
            let page = store
                .replica_snapshot_page(&owner_id, &position, 1)
                .unwrap();
            remaining.extend(page.items);
            let Some(next) = page.next else {
                break;
            };
            position = next;
        }
        let snapshot_settings = remaining
            .iter()
            .find_map(|item| match item {
                ReplicaProjection::Settings(value) => Some(value),
                _ => None,
            })
            .unwrap();
        assert_eq!(snapshot_settings.settings.revision, 1);
        assert_eq!(snapshot_settings.settings.timezone, "UTC");

        let (changes, latest, _) = store
            .replica_changes(
                &first.position.server_id,
                cut,
                &first.position.cut_token,
                10,
            )
            .unwrap();
        assert_eq!(latest, cut + 1);
        let ReplicaProjection::Settings(delta) = &changes[0].projection else {
            panic!("settings delta expected");
        };
        assert_eq!(delta.settings.revision, 2);
        assert_eq!(delta.settings.timezone, "UTC");

        assert!(matches!(
            store.replica_snapshot_page(&other.device_id, &position, 1),
            Err(StoreError::ReviewOwnership)
        ));
    }

    #[test]
    fn replica_feed_keeps_exact_revisions_and_tombstones() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let device = generate_device("Replica".into(), vec!["read".into()]).unwrap();
        let device_id = device.device_id.clone();
        store.create_device(&device, 1).unwrap();
        let initial = store.replica_state(&device_id).unwrap();

        let first = prepare_note(
            Some("document".into()),
            "note.org".into(),
            "#+title: First\n".into(),
            None,
        )
        .unwrap();
        let first_blob = BlobMeta {
            sha256: first.blob_sha256.clone(),
            size_bytes: first.org.len() as u64,
            media_type: "text/org; charset=utf-8".into(),
        };
        store.replace_note(&first, &first_blob, 2, None).unwrap();
        let second = prepare_note(
            Some("document".into()),
            "note.org".into(),
            "#+title: Second\n".into(),
            Some(1),
        )
        .unwrap();
        let second_blob = BlobMeta {
            sha256: second.blob_sha256.clone(),
            size_bytes: second.org.len() as u64,
            media_type: "text/org; charset=utf-8".into(),
        };
        store.replace_note(&second, &second_blob, 3, None).unwrap();
        store
            .tombstone_document(&DocumentId::new("document").unwrap(), 2, 4, None)
            .unwrap();

        let asset_bytes = b"asset";
        let asset_digest = format!("{:x}", sha2::Sha256::digest(asset_bytes));
        let asset_path = format!("assets/{asset_digest}/asset.png");
        let prepared = prepare_asset(asset_path.clone(), "asset.png".into(), None).unwrap();
        let asset_blob = BlobMeta {
            sha256: asset_digest,
            size_bytes: asset_bytes.len() as u64,
            media_type: "image/png".into(),
        };
        store.put_asset(&prepared, &asset_blob, 5, None).unwrap();
        store.tombstone_asset(&asset_path, 1, 6, None).unwrap();

        store
            .put_thema(&thema("First", None), 20260715, 7, None)
            .unwrap();
        store
            .put_thema(&thema("Second", Some(1)), 20260715, 8, None)
            .unwrap();

        let state = store.replica_state(&device_id).unwrap();
        let (changes, _, _) = store
            .replica_changes(&state.server_id, 0, &initial.cursor_token, 20)
            .unwrap();
        let documents = changes
            .iter()
            .filter_map(|change| match &change.projection {
                ReplicaProjection::Document(value) => Some(value),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(documents.len(), 3);
        assert_eq!(documents[0].projection.as_ref().unwrap().title, "First");
        assert_eq!(documents[1].projection.as_ref().unwrap().title, "Second");
        assert_eq!(documents[2].revision.operation, "tombstone");
        assert!(documents[2].projection.is_none());

        let assets = changes
            .iter()
            .filter_map(|change| match &change.projection {
                ReplicaProjection::Asset(value) => Some(value),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(assets.len(), 2);
        assert!(assets[0].blob_sha256.is_some());
        assert_eq!(assets[1].operation, "tombstone");
        assert!(assets[1].blob_sha256.is_none());

        let themata = changes
            .iter()
            .filter_map(|change| match &change.projection {
                ReplicaProjection::Thema(value) => Some(value),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(themata.len(), 2);
        assert_eq!(themata[0].record.content.keimenon, "First");
        assert_eq!(themata[1].record.content.keimenon, "Second");

        let review_states = changes
            .iter()
            .filter_map(|change| match &change.projection {
                ReplicaProjection::ReviewState(value) => Some(value),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(review_states.len(), 1);
        assert_eq!(review_states[0].state.state_revision, 0);
    }

    #[test]
    fn projection_only_documents_make_asset_tombstones_fail_closed() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        store
            .replace_document(
                &DocumentId::new("legacy").unwrap(),
                &projection("Legacy", &"a".repeat(64)),
                None,
            )
            .unwrap();
        let bytes = b"asset";
        let digest = format!("{:x}", sha2::Sha256::digest(bytes));
        let path = format!("assets/{digest}/asset.png");
        let asset = prepare_asset(path.clone(), "asset.png".into(), None).unwrap();
        let blob = BlobMeta {
            sha256: digest,
            size_bytes: bytes.len() as u64,
            media_type: "image/png".into(),
        };
        store.put_asset(&asset, &blob, 1, None).unwrap();

        assert!(matches!(
            store.tombstone_asset(&path, 1, 2, None),
            Err(StoreError::InvalidProjection { .. })
        ));
        assert!(store.asset_head(&path).unwrap().unwrap().active);
    }

    #[test]
    fn round_trips_duplicate_occurrences_and_unique_edges() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let id = DocumentId::new("document").unwrap();
        let inserted = store
            .replace_document(&id, &projection("Document", "digest"), None)
            .unwrap();
        let loaded = store.get_document(&id).unwrap().unwrap();
        assert_eq!(loaded, inserted);
        assert_eq!(loaded.projection().link_occurrences().len(), 2);
        assert_eq!(loaded.projection().semantic_edges().len(), 1);
    }

    #[test]
    fn identical_replacement_is_idempotent() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let id = DocumentId::new("document").unwrap();
        let projection = projection("Document", "digest");
        let first = store.replace_document(&id, &projection, None).unwrap();
        let second = store.replace_document(&id, &projection, None).unwrap();
        assert_eq!(first, second);
        assert_eq!(second.revision(), 1);
    }

    #[test]
    fn stale_revision_preserves_current_projection() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let id = DocumentId::new("document").unwrap();
        store
            .replace_document(&id, &projection("First", "digest-one"), None)
            .unwrap();
        let replacement = projection("Second", "digest-two");
        store.replace_document(&id, &replacement, Some(1)).unwrap();
        let error = store
            .replace_document(&id, &projection("Third", "digest-three"), Some(1))
            .unwrap_err();
        assert!(matches!(
            error,
            StoreError::Conflict {
                expected: 1,
                actual: Some(2)
            }
        ));
        assert_eq!(
            store.get_document(&id).unwrap().unwrap().projection(),
            &replacement
        );
    }
}
