use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior,
    backup::{Backup, StepResult},
    params,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::domain::{
    DocumentId, DocumentProjection, DocumentProjectionInput, DueThema, LinkOccurrence, Node,
    NodeEdge, NodeId, SourceSpan, ThemaConcept, ThemaConceptRelation, ThemaId, ThemaKind,
    TitlePath,
};
use crate::{
    application::{
        AssetHead, AssetRevision, ContentChange, ContentSearchGeneration, ContentSearchHit,
        ContentSearchItem, ContentSearchPage, DevicePrincipal, DeviceRecord, DocumentHead,
        DocumentProjectionSnapshot, DocumentReplicaRevision, DocumentRevision,
        MAX_DOCUMENT_PROJECTION_BYTES, MAX_DOCUMENT_SOURCE_BYTES, NewDevice, NodeCatalogPage,
        NodeCatalogRecord, NodeRecord, NodeThemaAssociation, PreparedAsset, PreparedNote,
        PreparedThema, ReplicaChange, ReplicaProjection, ReplicaState, ReviewStateRevision,
        SettingsInput, SettingsRecord, SettingsRevision, SnapshotPage, SnapshotPosition,
        ThemaCatalogPage, ThemaCatalogRecord, ThemaRecord, ThemaRevision, parse_stored_scopes,
        path_key, stored_scopes, stored_thema_content, validate_document_path,
    },
    blob::BlobMeta,
};

mod asset_sync;
mod client_views;
mod content_sync;
mod mutation;
mod notes_wipe;
mod review;

pub(crate) use mutation::DeviceMutationExecution;
pub(crate) use notes_wipe::NotesWipeSummary;

const CURRENT_SCHEMA: &str = include_str!("schema/server_v1.sql");
const APPLICATION_ID: u32 = 0x474e4f53;
const SCHEMA_VERSION: u32 = 4;
const THEMA_CATALOG_MATERIALIZED_BYTES: usize = 1024 * 1024;
const CONTENT_SEARCH_DOCUMENT_BUDGET: usize = 1_000;
const CONTENT_SEARCH_SOURCE_BUDGET: usize = 8 * 1024 * 1024;
const REPLICA_CHANGE_SCAN_BUDGET: usize = 4_096;
const REPLICA_CHANGE_SCAN_BATCH: usize = 256;
const BACKUP_STEP_DEADLINE: Duration = Duration::from_secs(30);
const BACKUP_RETRY_DELAY: Duration = Duration::from_millis(10);

#[derive(Debug, Error)]
pub(crate) enum StoreError {
    #[error("revision conflict: expected {expected}, current revision is {actual:?}")]
    Conflict { expected: u64, actual: Option<u64> },
    #[error("resource already exists at revision {actual}")]
    ExpectedAbsent { actual: u64 },
    #[error("resource was not found")]
    NotFound,
    #[error("another review session is active")]
    ActiveReviewSession,
    #[error("review attempt is stale")]
    StaleAttempt,
    #[error("review operation is invalid in the current state")]
    InvalidReviewState,
    #[error("topic traversal exceeded its work budget")]
    TopicTraversalLimitExceeded,
    #[error("database backup exceeded its retry deadline")]
    BackupDeadlineExceeded,
    #[error("database backup was cancelled")]
    BackupCancelled,
    #[error("review response is invalid for the captured presentation")]
    InvalidReviewResponse,
    #[error("review operation belongs to another device")]
    ReviewOwnership,
    #[error("operation identity was reused with a different payload")]
    IdempotencyConflict,
    #[error("device ID was reused with different provisioning data")]
    DeviceProvisioningConflict,
    #[error("device operation sequence has a gap: expected {expected}")]
    DeviceSequence { expected: u64 },
    #[error("replica cursor is outside the retained server feed")]
    ReplicaCursor,
    #[error("page cursor is not part of the requested collection")]
    InvalidCursor,
    #[error("node page projection budget was reached")]
    NodePageProjectionLimit,
    #[error("content catalog changed; restart search")]
    CatalogChanged,
    #[error("replica snapshot belongs to another server")]
    ReplicaServer,
    #[error("operation requires a staged blob")]
    BlobRequired { digest: String },
    #[error("blob metadata does not match stored content")]
    BlobMetadata,
    #[error("document path changes require a rename operation")]
    PathChange,
    #[error("logical path belongs to another active resource")]
    PathConflict,
    #[error("node ID {node_id} belongs to another document")]
    NodeIdConflict { node_id: String },
    #[error("thema references an inactive {kind}")]
    InactiveReference { kind: &'static str },
    #[error("document projection is invalid: {reason}")]
    InvalidProjection { reason: &'static str },
    #[error("stored document contains invalid {field}")]
    CorruptData { field: &'static str },
    #[error("canonical document content is unavailable or corrupt")]
    InvalidServerState,
    #[error("database schema version {found} is unsupported; expected version {supported}")]
    UnsupportedSchemaVersion { found: u32, supported: u32 },
    #[error("database schema layout is unsupported")]
    UnsupportedSchemaLayout,
    #[error("database application ID {found} is unsupported; expected {expected}")]
    UnsupportedApplicationId { found: u32, expected: u32 },
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

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredThemaConcept {
    node_id: String,
    relation: String,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DocumentAssetSource {
    pub(crate) document_id: String,
    pub(crate) content_type: String,
    pub(crate) blob_sha256: Option<String>,
    pub(crate) blob_size_bytes: Option<u64>,
    pub(crate) active_head: bool,
}

pub(crate) struct DocumentAssetAssociation {
    pub(crate) document_id: String,
    pub(crate) asset_paths: Vec<String>,
    pub(crate) active_head: bool,
}

pub(crate) struct SqliteStore {
    connection: Connection,
}

pub(super) enum AssetWriteDecision {
    Unchanged(AssetHead),
    Write,
}

pub(super) fn asset_write_decision(
    connection: &Connection,
    prepared: &PreparedAsset,
    blob: &BlobMeta,
    require_matching_blob_metadata: bool,
) -> Result<AssetWriteDecision, StoreError> {
    let collides: bool = connection
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
    let current: Option<(String, i64, bool)> = connection
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
    if require_matching_blob_metadata
        && prepared.expected_revision.is_none()
        && let Some((_, revision, _)) = current.as_ref()
    {
        return Err(StoreError::ExpectedAbsent {
            actual: u64::try_from(*revision).map_err(|_| StoreError::CorruptData {
                field: "asset revision",
            })?,
        });
    }
    let stored_blob: Option<(i64, String)> = connection
        .query_row(
            "SELECT size_bytes, media_type FROM blob WHERE content_sha256 = ?1",
            [&blob.sha256],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(StoreError::storage)?;
    if require_matching_blob_metadata
        && stored_blob.is_some_and(|(size, media_type)| {
            u64::try_from(size).ok() != Some(blob.size_bytes) || media_type != blob.media_type
        })
    {
        return Err(StoreError::BlobMetadata);
    }
    if current
        .as_ref()
        .is_some_and(|(digest, _, active)| digest == &blob.sha256 && *active)
    {
        return Ok(AssetWriteDecision::Unchanged(
            load_asset_head(connection, &prepared.logical_path)?.ok_or(StoreError::NotFound)?,
        ));
    }
    Ok(AssetWriteDecision::Write)
}

pub(super) fn put_asset_in(
    transaction: &Transaction<'_>,
    prepared: &PreparedAsset,
    blob: &BlobMeta,
    created_at_us: i64,
    author_device_id: Option<&str>,
    require_matching_blob_metadata: bool,
) -> Result<(AssetHead, bool), StoreError> {
    if let AssetWriteDecision::Unchanged(head) =
        asset_write_decision(transaction, prepared, blob, require_matching_blob_metadata)?
    {
        return Ok((head, false));
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
    let revision = next_number(current.as_ref().map(|(_, revision, _)| *revision))?;
    insert_blob(transaction, blob, created_at_us)?;
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
        transaction,
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
    Ok((
        load_asset_head(transaction, &prepared.logical_path)?.ok_or(StoreError::NotFound)?,
        true,
    ))
}

pub(super) fn tombstone_asset_in(
    transaction: &Transaction<'_>,
    asset_path: &str,
    expected_revision: u64,
    updated_at_us: i64,
    author_device_id: Option<&str>,
) -> Result<AssetHead, StoreError> {
    let current = load_asset_head(transaction, asset_path)?
        .filter(|asset| asset.active)
        .ok_or(StoreError::NotFound)?;
    check_expected_number(Some(expected_revision), Some(current.revision as i64))?;
    require_asset_unreferenced(transaction, asset_path)?;
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
        transaction,
        "asset",
        asset_path,
        revision,
        "tombstone",
        updated_at_us,
    )?;
    Ok(AssetHead {
        revision,
        blob_sha256: None,
        media_type: None,
        active: false,
        ..current
    })
}

pub(super) fn update_settings_in(
    transaction: &Transaction<'_>,
    input: &SettingsInput,
    expected_revision: u64,
    updated_at_us: i64,
    author_device_id: Option<&str>,
) -> Result<SettingsRecord, StoreError> {
    let current = transaction
        .query_row("SELECT revision FROM settings WHERE id = 1", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(StoreError::storage)?;
    check_expected_number(Some(expected_revision), Some(current))?;
    let revision = next_number(Some(current))?;
    let scheduler = input.scheduler();
    transaction
        .execute(
            "UPDATE settings SET revision = ?1, timezone = ?2,
                 day_boundary_hour = ?3, scheduler_json = ?4, updated_at_us = ?5
             WHERE id = 1",
            params![
                u64_to_i64(revision, "settings revision")?,
                input.timezone,
                i64::from(input.day_boundary_hour),
                serde_json::to_string(&scheduler).map_err(StoreError::storage)?,
                updated_at_us,
            ],
        )
        .map_err(StoreError::storage)?;
    let settings = load_settings(transaction)?;
    insert_settings_history(
        transaction,
        &settings,
        "update",
        updated_at_us,
        author_device_id,
    )?;
    insert_content_change(
        transaction,
        "settings",
        "canonical",
        revision,
        "update",
        updated_at_us,
    )?;
    Ok(settings)
}

pub(super) fn put_thema_in(
    transaction: &Transaction<'_>,
    prepared: &PreparedThema,
    initial_review_day: u32,
    updated_at_us: i64,
    author_device_id: Option<&str>,
    require_matching_expectation: bool,
) -> Result<(ThemaRecord, bool), StoreError> {
    let current = thema_revision(transaction, prepared.thema_id.as_str())?;
    let _ = (require_matching_expectation, &prepared.expected_revision);
    validate_thema_associations(transaction, prepared)?;
    if thema_is_unchanged(transaction, prepared)? {
        return Ok((
            load_thema(transaction, prepared.thema_id.as_str())?.ok_or(StoreError::NotFound)?,
            false,
        ));
    }
    let revision = next_number(current)?;
    upsert_thema_row(
        transaction,
        prepared,
        revision,
        updated_at_us,
        author_device_id,
    )?;
    replace_thema_associations(transaction, prepared)?;
    insert_initial_review_state(transaction, prepared, initial_review_day)?;
    let record =
        load_thema(transaction, prepared.thema_id.as_str())?.ok_or(StoreError::NotFound)?;
    insert_thema_history(
        transaction,
        &record,
        if current.is_some() {
            "update"
        } else {
            "create"
        },
        updated_at_us,
        author_device_id,
    )?;
    insert_initial_review_state_history(transaction, prepared, updated_at_us)?;
    insert_content_change(
        transaction,
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
            transaction,
            "review",
            prepared.thema_id.as_str(),
            0,
            "snapshot",
            updated_at_us,
        )?;
    }
    Ok((record, true))
}

pub(super) fn set_thema_archived_in(
    transaction: &Transaction<'_>,
    thema_id: &str,
    expected_revision: u64,
    archived: bool,
    updated_at_us: i64,
    author_device_id: Option<&str>,
) -> Result<ThemaRecord, StoreError> {
    let current = thema_revision(transaction, thema_id)?.ok_or(StoreError::NotFound)?;
    check_expected_number(Some(expected_revision), Some(current))?;
    if !archived {
        require_thema_associations_active(transaction, thema_id)?;
    }
    let revision = next_number(Some(current))?;
    transaction
        .execute(
            "UPDATE thema SET revision = ?2, archived_at_us = ?3, updated_at_us = ?4,
                 author_device_id = ?5
             WHERE thema_id = ?1",
            params![
                thema_id,
                u64_to_i64(revision, "thema revision")?,
                archived.then_some(updated_at_us),
                updated_at_us,
                author_device_id
            ],
        )
        .map_err(StoreError::storage)?;
    let record = load_thema(transaction, thema_id)?.ok_or(StoreError::NotFound)?;
    insert_thema_history(
        transaction,
        &record,
        if archived { "archive" } else { "unarchive" },
        updated_at_us,
        author_device_id,
    )?;
    insert_content_change(
        transaction,
        "thema",
        thema_id,
        revision,
        if archived { "archive" } else { "unarchive" },
        updated_at_us,
    )?;
    Ok(record)
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
        initialize_schema(&connection)?;
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

    pub(crate) fn due_legacy_themata_page(
        &self,
        day: u32,
        limit: u16,
        after: Option<(bool, u32, String)>,
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
        self.replace(document_id, projection, expected_revision, FailPoint::None)
    }

    pub(crate) fn replace_note(
        &mut self,
        prepared: &PreparedNote,
        blob: &BlobMeta,
        created_at_us: i64,
        author_device_id: Option<&str>,
    ) -> Result<StoredDocument, StoreError> {
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

    #[cfg(test)]
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
        let transaction = self.write_transaction()?;
        let (head, _) = put_asset_in(
            &transaction,
            prepared,
            blob,
            created_at_us,
            author_device_id,
            false,
        )?;
        transaction.commit().map_err(StoreError::storage)?;
        Ok(head.revision)
    }

    pub(crate) fn stage_blob(
        &mut self,
        blob: &BlobMeta,
        created_at_us: i64,
    ) -> Result<(), StoreError> {
        let transaction = self.write_transaction()?;
        let existing = transaction
            .query_row(
                "SELECT size_bytes FROM staged_blob WHERE content_sha256 = ?1",
                [&blob.sha256],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(StoreError::storage)?;
        if existing.is_some_and(|size| u64::try_from(size).ok() != Some(blob.size_bytes)) {
            return Err(StoreError::BlobMetadata);
        }
        transaction
            .execute(
                "INSERT INTO staged_blob(content_sha256, size_bytes, staged_at_us)
                 VALUES (?1, ?2, ?3) ON CONFLICT(content_sha256) DO NOTHING",
                params![
                    blob.sha256,
                    u64_to_i64(blob.size_bytes, "staged blob size")?,
                    created_at_us,
                ],
            )
            .map_err(StoreError::storage)?;
        transaction.commit().map_err(StoreError::storage)
    }

    pub(crate) fn document_head(
        &self,
        document_id: &DocumentId,
    ) -> Result<Option<DocumentHead>, StoreError> {
        self.connection
            .query_row(
                "SELECT document_id, logical_path, content_type, revision, head_blob_sha256, active
                 FROM document WHERE document_id = ?1",
                [document_id.as_str()],
                document_head_row,
            )
            .optional()
            .map_err(StoreError::storage)
    }

    pub(crate) fn document_blob_digests(&self) -> Result<Vec<String>, StoreError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT head_blob_sha256 FROM document WHERE head_blob_sha256 IS NOT NULL
                 UNION
                 SELECT blob_sha256 FROM document_revision WHERE blob_sha256 IS NOT NULL",
            )
            .map_err(StoreError::storage)?;
        let rows = statement
            .query_map([], |row| row.get(0))
            .map_err(StoreError::storage)?;
        rows.collect::<Result<Vec<_>, _>>()
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
                "SELECT document_id, revision, logical_path, content_type, blob_sha256, operation,
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
                "SELECT document_id, revision, logical_path, content_type, blob_sha256, operation,
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
        validate_document_path(logical_path).map_err(|_| StoreError::InvalidProjection {
            reason: "logical path is invalid",
        })?;
        let transaction = self.write_transaction()?;
        let content_type = load_document_head(&transaction, document_id)?
            .ok_or(StoreError::NotFound)?
            .content_type;
        crate::parser::validate_document_path(&content_type, logical_path).map_err(|_| {
            StoreError::InvalidProjection {
                reason: "logical path is invalid",
            }
        })?;
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
        let head = tombstone_asset_in(
            &transaction,
            asset_path,
            expected_revision,
            updated_at_us,
            author_device_id,
        )?;
        transaction.commit().map_err(StoreError::storage)?;
        Ok(head)
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
        let mut changes = Vec::new();
        let mut page_after = after;
        let mut scanned = 0;
        while changes.len() < usize::from(limit) {
            if scanned == REPLICA_CHANGE_SCAN_BUDGET {
                if changes.is_empty() {
                    return Err(StoreError::ReplicaCursor);
                }
                break;
            }
            let batch_limit =
                REPLICA_CHANGE_SCAN_BATCH.min(REPLICA_CHANGE_SCAN_BUDGET - scanned) as u16;
            let batch = self.content_changes(page_after, batch_limit)?;
            if batch.is_empty() {
                if scanned > 0 && changes.is_empty() {
                    return Err(StoreError::ReplicaCursor);
                }
                break;
            }
            scanned += batch.len();
            for change in batch {
                page_after = change.cursor;
                match load_replica_projection(
                    &self.connection,
                    &change.entity_kind,
                    &change.entity_id,
                    change.revision,
                ) {
                    Ok(projection) => {
                        changes.push(ReplicaChange {
                            cursor: change.cursor,
                            cursor_token: change.feed_token,
                            projection,
                        });
                        if changes.len() == usize::from(limit) {
                            break;
                        }
                    }
                    Err(StoreError::ReplicaCursor) => {}
                    Err(error) => return Err(error),
                }
            }
        }
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
                load_snapshot_projection(&self.connection, kind, id, *revision)
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

    #[cfg(test)]
    pub(crate) fn backup_database(source: &Path, destination: &Path) -> Result<(), StoreError> {
        let cancelled = AtomicBool::new(false);
        Self::backup_database_cancellable(source, destination, &cancelled)
    }

    pub(crate) fn backup_database_cancellable(
        source: &Path,
        destination: &Path,
        cancelled: &AtomicBool,
    ) -> Result<(), StoreError> {
        Self::backup_database_cancellable_with_deadline(
            source,
            destination,
            BACKUP_STEP_DEADLINE,
            cancelled,
        )
    }

    pub(crate) fn backup_database_cancellable_with_deadline(
        source: &Path,
        destination: &Path,
        retry_budget: Duration,
        cancelled: &AtomicBool,
    ) -> Result<(), StoreError> {
        backup_database_with_step(source, destination, retry_budget, cancelled, |_| {})
    }

    #[cfg(test)]
    pub(crate) fn referenced_blobs(&self) -> Result<Vec<(String, u64)>, StoreError> {
        load_blob_rows(&self.connection)
    }

    pub(crate) fn blobs_page(
        &self,
        after: Option<&str>,
        limit: u16,
    ) -> Result<Vec<(String, u64)>, StoreError> {
        load_digest_rows_page(&self.connection, "blob", after, limit)
    }

    pub(crate) fn has_blob_record(&self, digest: &str) -> Result<bool, StoreError> {
        self.connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM blob WHERE content_sha256 = ?1)",
                [digest],
                |row| row.get(0),
            )
            .map_err(StoreError::storage)
    }

    pub(crate) fn has_blob_reference(&self, digest: &str) -> Result<bool, StoreError> {
        self.connection
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM blob WHERE content_sha256 = ?1
                     UNION ALL
                     SELECT 1 FROM staged_blob WHERE content_sha256 = ?1
                     UNION ALL
                     SELECT 1 FROM offline_asset_evidence WHERE content_sha256 = ?1
                 )",
                [digest],
                |row| row.get(0),
            )
            .map_err(StoreError::storage)
    }

    pub(crate) fn staged_blobs_page(
        &self,
        after: Option<&str>,
        limit: u16,
    ) -> Result<Vec<(String, u64)>, StoreError> {
        load_digest_rows_page(&self.connection, "staged_blob", after, limit)
    }

    pub(crate) fn offline_asset_evidence_page(
        &self,
        after: Option<&str>,
        limit: u16,
    ) -> Result<Vec<(String, String, u64)>, StoreError> {
        load_offline_asset_evidence_page(&self.connection, after, limit)
    }

    pub(crate) fn document_asset_heads_page(
        &self,
        after: Option<&str>,
        limit: u16,
    ) -> Result<Vec<DocumentAssetSource>, StoreError> {
        load_document_asset_heads_page(&self.connection, after, limit)
    }

    pub(crate) fn repair_document_assets(
        &mut self,
        associations: &[DocumentAssetAssociation],
    ) -> Result<(), StoreError> {
        repair_document_asset_rows(&mut self.connection, associations)
    }

    #[cfg(test)]
    pub(crate) fn verify_database_file(path: &Path) -> Result<Vec<(String, u64)>, StoreError> {
        Self::verify_database_file_cancellable(path, Arc::new(AtomicBool::new(false)))
    }

    pub(crate) fn verify_database_file_cancellable(
        path: &Path,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Vec<(String, u64)>, StoreError> {
        Self::verify_database_file_cancellable_with(path, cancelled, || {})
    }

    fn verify_database_file_cancellable_with(
        path: &Path,
        cancelled: Arc<AtomicBool>,
        active: impl FnOnce(),
    ) -> Result<Vec<(String, u64)>, StoreError> {
        if cancelled.load(Ordering::Acquire) {
            return Err(StoreError::BackupCancelled);
        }
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(StoreError::storage)?;
        let progress_cancelled = Arc::clone(&cancelled);
        connection.progress_handler(
            1_000,
            Some(move || progress_cancelled.load(Ordering::Acquire)),
        );
        active();
        let result = (|| {
            let version = schema_version(&connection)?;
            if version != SCHEMA_VERSION {
                return Err(StoreError::UnsupportedSchemaVersion {
                    found: version,
                    supported: SCHEMA_VERSION,
                });
            }
            require_application_id(&connection)?;
            require_current_schema_layout(&connection)?;
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
            load_blob_rows_cancellable(&connection, &cancelled)
        })();
        if cancelled.load(Ordering::Acquire) {
            Err(StoreError::BackupCancelled)
        } else {
            result
        }
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
        let settings = update_settings_in(
            &transaction,
            input,
            expected_revision,
            updated_at_us,
            author_device_id,
        )?;
        transaction.commit().map_err(StoreError::storage)?;
        Ok(settings)
    }

    pub(crate) fn put_thema(
        &mut self,
        prepared: &PreparedThema,
        initial_review_day: u32,
        updated_at_us: i64,
        author_device_id: Option<&str>,
    ) -> Result<ThemaRecord, StoreError> {
        let transaction = self.write_transaction()?;
        let (record, _) = put_thema_in(
            &transaction,
            prepared,
            initial_review_day,
            updated_at_us,
            author_device_id,
            false,
        )?;
        transaction.commit().map_err(StoreError::storage)?;
        Ok(record)
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
        let record = set_thema_archived_in(
            &transaction,
            thema_id,
            expected_revision,
            archived,
            updated_at_us,
            author_device_id,
        )?;
        transaction.commit().map_err(StoreError::storage)?;
        Ok(record)
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

    pub(crate) fn thema_catalog(
        &self,
        device_id: &str,
        query: &str,
        limit: u16,
        after: Option<&str>,
    ) -> Result<ThemaCatalogPage, StoreError> {
        self.thema_catalog_with_byte_budget(
            device_id,
            query,
            limit,
            after,
            THEMA_CATALOG_MATERIALIZED_BYTES,
        )
    }

    fn thema_catalog_with_byte_budget(
        &self,
        device_id: &str,
        query: &str,
        limit: u16,
        after: Option<&str>,
        byte_budget: usize,
    ) -> Result<ThemaCatalogPage, StoreError> {
        let state = self.replica_state(device_id)?;
        let pattern = format!("%{}%", query.to_lowercase());
        let mut statement = self
            .connection
            .prepare(
                "SELECT DISTINCT thema.thema_id,
                        length(CAST(thema.thema_id AS BLOB))
                        + length(CAST(thema.kind AS BLOB))
                        + length(CAST(thema.keimenon AS BLOB))
                        + length(CAST(thema.hypothesis_json AS BLOB))
                        + length(CAST(thema.answer_json AS BLOB))
                        + coalesce((
                            SELECT sum(length(CAST(tags.tag AS BLOB)))
                            FROM thema_tag AS tags WHERE tags.thema_id = thema.thema_id
                          ), 0) + 64
                 FROM thema
                 JOIN review_state USING(thema_id)
                 LEFT JOIN thema_tag USING(thema_id)
                 WHERE thema.thema_id > coalesce(?2, '')
                   AND (lower(thema.keimenon) LIKE ?1 OR lower(thema_tag.tag) LIKE ?1)
                 ORDER BY thema.thema_id LIMIT ?3",
            )
            .map_err(StoreError::storage)?;
        let rows = statement
            .query_map(params![pattern, after, i64::from(limit) + 1], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(StoreError::storage)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::storage)?;
        drop(statement);
        let mut items = Vec::new();
        let mut materialized_bytes = 0usize;
        let mut has_more = false;
        for (thema_id, projected_bytes) in rows {
            let projected_bytes =
                usize::try_from(projected_bytes).map_err(|_| StoreError::CorruptData {
                    field: "thema catalog size",
                })?;
            if items.len() == usize::from(limit)
                || (!items.is_empty()
                    && materialized_bytes.saturating_add(projected_bytes) > byte_budget)
            {
                has_more = true;
                break;
            }
            items.push(load_thema_catalog_record(&self.connection, &thema_id)?);
            materialized_bytes = materialized_bytes.saturating_add(projected_bytes);
        }
        Ok(ThemaCatalogPage {
            state,
            items,
            has_more,
        })
    }

    pub(crate) fn get_node(&self, node_id: &str) -> Result<Option<NodeRecord>, StoreError> {
        load_node(&self.connection, node_id)
    }

    pub(crate) fn node_themata(
        &self,
        node_id: &str,
        limit: u16,
        after: Option<&str>,
    ) -> Result<Vec<NodeThemaAssociation>, StoreError> {
        let node_exists: bool = self
            .connection
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM node WHERE node_id = ?1
                     UNION ALL SELECT 1 FROM retired_node WHERE node_id = ?1
                 )",
                params![node_id],
                |row| row.get(0),
            )
            .map_err(StoreError::storage)?;
        if !node_exists {
            return Err(StoreError::NotFound);
        }
        if let Some(after) = after {
            let associated: bool = self
                .connection
                .query_row(
                    "SELECT EXISTS(
                         SELECT 1 FROM thema_concept
                         WHERE concept_id = ?1 AND thema_id = ?2
                     )",
                    params![node_id, after],
                    |row| row.get(0),
                )
                .map_err(StoreError::storage)?;
            if !associated {
                return Err(StoreError::InvalidCursor);
            }
        }
        let mut statement = self
            .connection
            .prepare(
                "SELECT thema_id, relation FROM thema_concept
                 WHERE concept_id = ?1 AND thema_id > coalesce(?2, '')
                 ORDER BY thema_id LIMIT ?3",
            )
            .map_err(StoreError::storage)?;
        statement
            .query_map(params![node_id, after, i64::from(limit)], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(StoreError::storage)?
            .map(|row| {
                let (thema_id, relation) = row.map_err(StoreError::storage)?;
                Ok(NodeThemaAssociation {
                    thema_id,
                    relation: ThemaConceptRelation::parse(&relation).ok_or(
                        StoreError::CorruptData {
                            field: "thema concept relation",
                        },
                    )?,
                })
            })
            .collect()
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
        load_node_records(&self.connection, ids)
    }

    pub(crate) fn search_nodes_by_tag(
        &self,
        tag: &str,
        limit: u16,
        after: Option<&str>,
    ) -> Result<Vec<NodeRecord>, StoreError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT node_id FROM (
                     SELECT DISTINCT node.node_id FROM node
                     JOIN node_tag ON node_tag.document_id = node.document_id
                                      AND node_tag.node_id = node.node_id
                     WHERE node_tag.tag = ?1
                     UNION
                     SELECT retired.node_id FROM retired_node AS retired
                     JOIN json_each(retired.tags_json) AS tag
                     WHERE tag.value = ?1
                       AND NOT EXISTS(SELECT 1 FROM node WHERE node_id = retired.node_id)
                 ) WHERE node_id > coalesce(?2, '')
                 ORDER BY node_id LIMIT ?3",
            )
            .map_err(StoreError::storage)?;
        let ids = statement
            .query_map(params![tag, after, i64::from(limit)], |row| row.get(0))
            .map_err(StoreError::storage)?
            .collect::<Result<Vec<String>, _>>()
            .map_err(StoreError::storage)?;
        load_node_records(&self.connection, ids)
    }

    pub(crate) fn search_document_content(
        &mut self,
        query: &str,
        expected_generation: Option<&ContentSearchGeneration>,
        after: Option<&str>,
        limit: u16,
        mut load_source: impl FnMut(&str, usize) -> Result<Vec<u8>, StoreError>,
    ) -> Result<ContentSearchPage, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(StoreError::storage)?;
        let cursor = current_content_cursor(&transaction)?;
        let generation = ContentSearchGeneration {
            server_id: server_id(&transaction)?,
            cursor,
            cursor_token: replica_cursor_token(&transaction, cursor)?,
        };
        if let Some(expected) = expected_generation {
            if expected.server_id == generation.server_id {
                match replica_cursor_token(&transaction, expected.cursor) {
                    Ok(token) if token != expected.cursor_token => {
                        return Err(StoreError::InvalidCursor);
                    }
                    Err(StoreError::ReplicaCursor) => return Err(StoreError::CatalogChanged),
                    Err(error) => return Err(error),
                    Ok(_) => {}
                }
            }
            if expected != &generation {
                return Err(StoreError::CatalogChanged);
            }
        }
        if let Some(after) = after {
            let valid: bool = transaction
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM document WHERE document_id = ?1 AND active = 1)",
                    [after],
                    |row| row.get(0),
                )
                .map_err(StoreError::storage)?;
            if !valid {
                return Err(StoreError::InvalidCursor);
            }
        }
        let rows = {
            let mut statement = transaction
                .prepare(
                    "SELECT document.document_id, document.logical_path, document.revision,
                            document.content_sha256, document.head_blob_sha256,
                            document.file_node_id, blob.size_bytes
                     FROM document
                     LEFT JOIN blob ON blob.content_sha256 = document.head_blob_sha256
                     WHERE document.active = 1
                       AND document.document_id > coalesce(?1, '')
                     ORDER BY document.document_id
                     LIMIT ?2",
                )
                .map_err(StoreError::storage)?;
            statement
                .query_map(
                    params![after, CONTENT_SEARCH_DOCUMENT_BUDGET as i64 + 1],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, Option<String>>(4)?,
                            row.get::<_, Option<String>>(5)?,
                            row.get::<_, Option<i64>>(6)?,
                        ))
                    },
                )
                .map_err(StoreError::storage)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::storage)?
        };
        let matcher = regex::RegexBuilder::new(&regex::escape(query))
            .case_insensitive(true)
            .build()
            .map_err(StoreError::storage)?;
        let mut source_matches = HashMap::<String, bool>::new();
        let mut source_bytes = 0_usize;
        let mut items = Vec::new();
        let mut scanned_after = after.map(str::to_owned);
        let mut has_more = rows.len() > CONTENT_SEARCH_DOCUMENT_BUDGET;
        for row in rows.into_iter().take(CONTENT_SEARCH_DOCUMENT_BUDGET) {
            if items.len() == usize::from(limit) {
                has_more = true;
                break;
            }
            let (
                document_id,
                logical_path,
                revision,
                content_digest,
                head_digest,
                file_node_id,
                size,
            ) = row;
            let logical_path = logical_path.ok_or(StoreError::InvalidServerState)?;
            let head_digest = head_digest.ok_or(StoreError::InvalidServerState)?;
            if head_digest != content_digest {
                return Err(StoreError::InvalidServerState);
            }
            let size = usize::try_from(size.ok_or(StoreError::InvalidServerState)?)
                .map_err(|_| StoreError::InvalidServerState)?;
            if size > MAX_DOCUMENT_SOURCE_BYTES {
                return Err(StoreError::InvalidServerState);
            }
            let matches = if let Some(matches) = source_matches.get(&content_digest) {
                *matches
            } else {
                if source_bytes
                    .checked_add(size)
                    .is_none_or(|total| total > CONTENT_SEARCH_SOURCE_BUDGET)
                {
                    has_more = true;
                    break;
                }
                let source = load_source(&content_digest, size)?;
                if source.len() != size {
                    return Err(StoreError::InvalidServerState);
                }
                source_bytes += size;
                let source =
                    std::str::from_utf8(&source).map_err(|_| StoreError::InvalidServerState)?;
                let matches = matcher.is_match(source);
                source_matches.insert(content_digest.clone(), matches);
                matches
            };
            let resume_after = scanned_after.clone();
            scanned_after = Some(document_id.clone());
            if matches {
                items.push(ContentSearchItem {
                    hit: ContentSearchHit {
                        document_id,
                        logical_path,
                        revision: u64::try_from(revision)
                            .ok()
                            .filter(|revision| *revision > 0)
                            .ok_or(StoreError::InvalidServerState)?,
                        content_digest,
                        file_node_id,
                    },
                    resume_after,
                });
            }
        }
        let ending_cursor = current_content_cursor(&transaction)?;
        let ending_generation = ContentSearchGeneration {
            server_id: server_id(&transaction)?,
            cursor: ending_cursor,
            cursor_token: replica_cursor_token(&transaction, ending_cursor)?,
        };
        if ending_generation != generation {
            return Err(StoreError::CatalogChanged);
        }
        transaction.commit().map_err(StoreError::storage)?;
        Ok(ContentSearchPage {
            generation,
            items,
            next_position: has_more.then_some(scanned_after).flatten(),
        })
    }

    pub(crate) fn node_catalog(
        &self,
        device_id: &str,
        document_id: Option<&str>,
        limit: u16,
        after: Option<&str>,
    ) -> Result<NodeCatalogPage, StoreError> {
        let state = self.replica_state(device_id)?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT node_id, document_id, logical_path, title_path_json, active
                 FROM (
                     SELECT node.node_id, node.document_id, document.logical_path,
                            node.title_path_json, node.active
                     FROM node JOIN document USING(document_id)
                     UNION ALL
                     SELECT retired.node_id, retired.document_id, retired.logical_path,
                            retired.title_path_json, 0
                     FROM retired_node AS retired
                     WHERE NOT EXISTS(
                         SELECT 1 FROM node WHERE node_id = retired.node_id
                     )
                 )
                 WHERE (?1 IS NULL OR document_id = ?1)
                   AND node_id > coalesce(?2, '')
                 ORDER BY node_id LIMIT ?3",
            )
            .map_err(StoreError::storage)?;
        let rows = statement
            .query_map(
                params![document_id, after, i64::from(limit) + 1],
                |row| -> rusqlite::Result<_> {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, bool>(4)?,
                    ))
                },
            )
            .map_err(StoreError::storage)?;
        let mut items = rows
            .map(|row| {
                let (node_id, document_id, logical_path, title_path, active) =
                    row.map_err(StoreError::storage)?;
                Ok(NodeCatalogRecord {
                    node_id,
                    document_id,
                    logical_path,
                    title_path: from_json(&title_path, "node title path")?,
                    active,
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        let has_more = items.len() > usize::from(limit);
        items.truncate(usize::from(limit));
        Ok(NodeCatalogPage {
            state,
            items,
            has_more,
        })
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
        load_node_records(&self.connection, ids)
    }

    pub(crate) fn create_device(
        &mut self,
        device: &NewDevice,
        created_at_us: i64,
    ) -> Result<DeviceRecord, StoreError> {
        let scopes = stored_scopes(&device.scopes).map_err(StoreError::storage)?;
        let transaction = self.write_transaction()?;
        let existing = transaction
            .query_row(
                "SELECT name, token_digest, scopes_json FROM device WHERE device_id = ?1",
                [&device.device_id],
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
        if let Some((name, token_digest, stored_scopes)) = existing {
            transaction.commit().map_err(StoreError::storage)?;
            if name != device.name || token_digest != device.token_digest || stored_scopes != scopes
            {
                return Err(StoreError::DeviceProvisioningConflict);
            }
            return self.device(&device.device_id)?.ok_or(StoreError::NotFound);
        }
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
        let _ = expected_revision;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(StoreError::storage)?;
        let current = load_document(&transaction, document_id)?;
        if current
            .as_ref()
            .is_some_and(|stored| stored.projection() == projection)
        {
            transaction.commit().map_err(StoreError::storage)?;
            return current.ok_or(StoreError::CorruptData { field: "document" });
        }
        let revision = next_revision(current.as_ref())?;
        replace_rows(
            &transaction,
            document_id,
            projection,
            crate::parser::ORG_CONTENT_TYPE,
            revision,
            fail_point,
        )?;
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
    device_id: &str,
) -> Result<bool, StoreError> {
    let operation_id = scoped_operation_id(device_id, operation_id);
    let existing = connection
        .query_row(
            "SELECT operation_kind, payload_sha256
             FROM device_operation WHERE operation_id = ?1",
            [&operation_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(StoreError::storage)?;
    let Some((stored_kind, stored_digest)) = existing else {
        return Ok(false);
    };
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
    let operation_id = scoped_operation_id(operation.device_id, operation.operation_id);
    transaction
        .execute(
            "INSERT INTO device_operation(
                 operation_id, device_id, device_sequence, operation_kind,
                 format_version, payload_sha256, payload_json, received_at_us
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                operation_id,
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

fn scoped_operation_id(device_id: &str, operation_id: &str) -> String {
    format!("{}:{device_id}{operation_id}", device_id.len())
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
        content_type: row.get(2)?,
        revision: row_u64(row, 3)?,
        blob_sha256: row.get(4)?,
        active: row.get::<_, i64>(5)? != 0,
    })
}

fn document_revision_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DocumentRevision> {
    Ok(DocumentRevision {
        document_id: row.get(0)?,
        revision: row_u64(row, 1)?,
        logical_path: row.get(2)?,
        content_type: row.get(3)?,
        blob_sha256: row.get(4)?,
        operation: row.get(5)?,
        created_at_us: row_u64(row, 6)?,
        author_device_id: row.get(7)?,
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
                        document_id AS entity_id, revision
                 FROM document
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
        "document" => load_document_replica_head(connection, id, Some(revision))
            .map(ReplicaProjection::Document),
        "asset" => load_asset_revision(connection, id, revision).map(ReplicaProjection::Asset),
        "thema" => {
            load_thema_replica_head(connection, id, Some(revision)).map(ReplicaProjection::Thema)
        }
        "settings" => load_settings_revision(connection, revision).map(ReplicaProjection::Settings),
        "review" => {
            load_review_state_revision(connection, id, revision).map(ReplicaProjection::ReviewState)
        }
        _ => Err(StoreError::CorruptData {
            field: "content change kind",
        }),
    }
}

fn load_snapshot_projection(
    connection: &Connection,
    kind: &str,
    id: &str,
    revision: u64,
) -> Result<ReplicaProjection, StoreError> {
    match kind {
        "document" => load_document_replica_head(connection, id, None).and_then(|head| {
            if head.revision.revision < revision {
                Err(StoreError::InvalidServerState)
            } else {
                Ok(ReplicaProjection::Document(head))
            }
        }),
        "thema" => load_thema_replica_head(connection, id, None).and_then(|head| {
            if head.record.revision < revision {
                Err(StoreError::InvalidServerState)
            } else {
                Ok(ReplicaProjection::Thema(head))
            }
        }),
        _ => load_replica_projection(connection, kind, id, revision),
    }
}

fn load_document_replica_head(
    connection: &Connection,
    id: &str,
    expected_revision: Option<u64>,
) -> Result<DocumentReplicaRevision, StoreError> {
    let document_id = DocumentId::new(id.to_owned()).map_err(|_| StoreError::CorruptData {
        field: "document ID",
    })?;
    let head = load_document_head(connection, &document_id)?.ok_or(StoreError::ReplicaCursor)?;
    if expected_revision.is_some_and(|revision| head.revision != revision) {
        return Err(StoreError::ReplicaCursor);
    }
    let revision = head.revision;
    let fallback_operation = if head.active {
        if revision == 1 { "create" } else { "update" }
    } else {
        "tombstone"
    };
    let (updated_at_us, author_device_id) = connection
        .query_row(
            "SELECT updated_at_us, author_device_id FROM document WHERE document_id = ?1",
            [id],
            |row| Ok((row_u64(row, 0)?, row.get(1)?)),
        )
        .map_err(StoreError::storage)?;
    let (operation, created_at_us) = current_change_metadata(
        connection,
        "document",
        id,
        revision,
        fallback_operation,
        updated_at_us,
    )?;
    let projection = head
        .active
        .then(|| load_document(connection, &document_id))
        .transpose()?
        .flatten()
        .map(|document| DocumentProjectionSnapshot::from(document.projection()));
    if head.active && projection.is_none() {
        return Err(StoreError::CorruptData {
            field: "document projection",
        });
    }
    Ok(DocumentReplicaRevision {
        revision: DocumentRevision {
            document_id: head.document_id,
            revision: head.revision,
            logical_path: head.logical_path,
            content_type: head.content_type,
            blob_sha256: head.blob_sha256,
            operation,
            created_at_us,
            author_device_id,
        },
        projection,
    })
}

fn load_thema_replica_head(
    connection: &Connection,
    id: &str,
    expected_revision: Option<u64>,
) -> Result<ThemaRevision, StoreError> {
    let record = load_thema(connection, id)?.ok_or(StoreError::ReplicaCursor)?;
    if expected_revision.is_some_and(|revision| record.revision != revision) {
        return Err(StoreError::ReplicaCursor);
    }
    let revision = record.revision;
    let fallback_operation = if revision == 1 { "create" } else { "update" };
    let (updated_at_us, author_device_id) = connection
        .query_row(
            "SELECT updated_at_us, author_device_id FROM thema WHERE thema_id = ?1",
            [id],
            |row| Ok((row_u64(row, 0)?, row.get(1)?)),
        )
        .map_err(StoreError::storage)?;
    let (operation, created_at_us) = current_change_metadata(
        connection,
        "thema",
        id,
        revision,
        fallback_operation,
        updated_at_us,
    )?;
    Ok(ThemaRevision {
        record,
        operation,
        created_at_us,
        author_device_id,
    })
}

fn current_change_metadata(
    connection: &Connection,
    kind: &str,
    id: &str,
    revision: u64,
    fallback_operation: &str,
    fallback_created_at_us: u64,
) -> Result<(String, u64), StoreError> {
    connection
        .query_row(
            "SELECT operation, created_at_us FROM content_change
             WHERE entity_kind = ?1 AND entity_id = ?2 AND revision = ?3
             ORDER BY cursor DESC LIMIT 1",
            params![kind, id, u64_to_i64(revision, "content revision")?],
            |row| Ok((row.get(0)?, row_u64(row, 1)?)),
        )
        .optional()
        .map_err(StoreError::storage)
        .map(|metadata| {
            metadata.unwrap_or_else(|| (fallback_operation.to_owned(), fallback_created_at_us))
        })
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
            "SELECT document_id, revision, logical_path, content_type, blob_sha256, operation,
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

#[allow(dead_code)]
fn load_thema_revision(
    connection: &Connection,
    id: &str,
    revision: u64,
) -> Result<ThemaRevision, StoreError> {
    let row = connection
        .query_row(
            "SELECT thema_id, revision, kind, keimenon, hypothesis_json, answer_json,
                    source_guid, parathema, review_image, tags_json, concepts_json,
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
    let concepts = parse_stored_concepts(&row.0, &row.10)?;
    Ok(ThemaRevision {
        record: ThemaRecord {
            thema_id: row.0,
            revision: to_u64(row.1, "thema revision")?,
            content,
            tags: serde_json::from_str(&row.9).map_err(StoreError::storage)?,
            concepts,
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
            "SELECT revision, timezone, day_boundary_hour, scheduler_json,
                    operation, created_at_us, author_device_id
             FROM settings_revision WHERE revision = ?1",
            [u64_to_i64(revision, "settings revision")?],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            },
        )
        .optional()
        .map_err(StoreError::storage)?
        .ok_or(StoreError::ReplicaCursor)?;
    let scheduler = serde_json::from_str(&row.3).map_err(StoreError::storage)?;
    Ok(SettingsRevision {
        settings: SettingsRecord {
            revision: to_u64(row.0, "settings revision")?,
            timezone: row.1,
            day_boundary_hour: u8::try_from(row.2).map_err(|_| StoreError::CorruptData {
                field: "day boundary",
            })?,
            scheduler,
        },
        operation: row.4,
        created_at_us: to_u64(row.5, "settings revision timestamp")?,
        author_device_id: row.6,
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
    let (revision, timezone, boundary, scheduler): (i64, String, i64, String) = connection
        .query_row(
            "SELECT revision, timezone, day_boundary_hour, scheduler_json
             FROM settings WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .map_err(StoreError::storage)?;
    let scheduler = serde_json::from_str(&scheduler).map_err(StoreError::storage)?;
    Ok(SettingsRecord {
        revision: u64::try_from(revision).map_err(|_| StoreError::CorruptData {
            field: "settings revision",
        })?,
        timezone,
        day_boundary_hour: u8::try_from(boundary).map_err(|_| StoreError::CorruptData {
            field: "day boundary",
        })?,
        scheduler,
    })
}

#[cfg(test)]
fn load_blob_rows(connection: &Connection) -> Result<Vec<(String, u64)>, StoreError> {
    load_blob_rows_cancellable(connection, &AtomicBool::new(false))
}

fn load_blob_rows_cancellable(
    connection: &Connection,
    cancelled: &AtomicBool,
) -> Result<Vec<(String, u64)>, StoreError> {
    let mut statement = connection
        .prepare(
            "SELECT content_sha256, size_bytes FROM blob
             UNION ALL SELECT content_sha256, size_bytes FROM staged_blob
             UNION ALL SELECT content_sha256, size_bytes FROM offline_asset_evidence
             ORDER BY content_sha256, size_bytes",
        )
        .map_err(StoreError::storage)?;
    let rows = statement
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row_u64(row, 1)?)))
        .map_err(StoreError::storage)?;
    let mut blobs = Vec::new();
    for row in rows {
        if cancelled.load(Ordering::Acquire) {
            return Err(StoreError::BackupCancelled);
        }
        let (digest, size) = row.map_err(StoreError::storage)?;
        if let Some((previous_digest, previous_size)) = blobs.last()
            && previous_digest == &digest
        {
            if previous_size != &size {
                return Err(StoreError::BlobMetadata);
            }
            continue;
        }
        blobs.push((digest, size));
    }
    Ok(blobs)
}

fn load_digest_rows_page(
    connection: &Connection,
    table: &str,
    after: Option<&str>,
    limit: u16,
) -> Result<Vec<(String, u64)>, StoreError> {
    debug_assert!(matches!(table, "blob" | "staged_blob"));
    let mut statement = connection
        .prepare(&format!(
            "SELECT content_sha256, size_bytes FROM {table}
             WHERE content_sha256 > coalesce(?1, '')
             ORDER BY content_sha256 LIMIT ?2"
        ))
        .map_err(StoreError::storage)?;
    statement
        .query_map(params![after, i64::from(limit)], |row| {
            Ok((row.get::<_, String>(0)?, row_u64(row, 1)?))
        })
        .map_err(StoreError::storage)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::storage)
}

fn load_offline_asset_evidence_page(
    connection: &Connection,
    after: Option<&str>,
    limit: u16,
) -> Result<Vec<(String, String, u64)>, StoreError> {
    let mut statement = connection
        .prepare(
            "SELECT operation_id, content_sha256, size_bytes FROM offline_asset_evidence
             WHERE operation_id > coalesce(?1, '') ORDER BY operation_id LIMIT ?2",
        )
        .map_err(StoreError::storage)?;
    statement
        .query_map(params![after, i64::from(limit)], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row_u64(row, 2)?,
            ))
        })
        .map_err(StoreError::storage)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::storage)
}

fn load_document_asset_heads_page(
    connection: &Connection,
    after: Option<&str>,
    limit: u16,
) -> Result<Vec<DocumentAssetSource>, StoreError> {
    let mut statement = connection
        .prepare(
            "SELECT document.document_id, document.head_blob_sha256,
                    document.content_type, blob.size_bytes, document.active = 1
             FROM document
             LEFT JOIN blob ON blob.content_sha256 = document.head_blob_sha256
             WHERE document.document_id > coalesce(?1, '')
             ORDER BY document.document_id LIMIT ?2",
        )
        .map_err(StoreError::storage)?;
    statement
        .query_map(params![after, i64::from(limit)], |row| {
            Ok(DocumentAssetSource {
                document_id: row.get(0)?,
                blob_sha256: row.get(1)?,
                content_type: row.get(2)?,
                blob_size_bytes: row_optional_u64(row, 3)?,
                active_head: row.get(4)?,
            })
        })
        .map_err(StoreError::storage)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::storage)
}

fn repair_document_asset_rows(
    connection: &mut Connection,
    associations: &[DocumentAssetAssociation],
) -> Result<(), StoreError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(StoreError::storage)?;
    for association in associations {
        let document_id = DocumentId::new(association.document_id.clone()).map_err(|_| {
            StoreError::CorruptData {
                field: "document ID",
            }
        })?;
        transaction
            .execute(
                "DELETE FROM document_asset WHERE document_id = ?1",
                [document_id.as_str()],
            )
            .map_err(StoreError::storage)?;
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

fn row_optional_u64(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Option<u64>> {
    row.get::<_, Option<i64>>(index)?
        .map(u64::try_from)
        .transpose()
        .map_err(|error| {
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
            "SELECT document_id, logical_path, content_type, revision, head_blob_sha256, active
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
    crate::parser::validate_document_path(&current.content_type, logical_path).map_err(|_| {
        StoreError::InvalidProjection {
            reason: "logical path is invalid",
        }
    })?;
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
        author_device_id,
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
    retire_document(
        transaction,
        document_id,
        revision,
        updated_at_us,
        author_device_id,
    )?;
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
    author_device_id: Option<&str>,
) -> Result<(), StoreError> {
    transaction
        .execute(
            "UPDATE document SET active = 0, head_blob_sha256 = NULL,
                 revision = ?2, updated_at_us = ?3, author_device_id = ?4
             WHERE document_id = ?1",
            params![
                document_id.as_str(),
                u64_to_i64(revision, "document revision")?,
                updated_at_us,
                author_device_id
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
            "DELETE FROM document_projection WHERE document_id = ?1",
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
        Err(StoreError::PathConflict)
    }
}

fn update_document_path(
    transaction: &Transaction<'_>,
    document_id: &DocumentId,
    logical_path: &str,
    revision: u64,
    updated_at_us: i64,
    author_device_id: Option<&str>,
) -> Result<(), StoreError> {
    transaction
        .execute(
            "UPDATE document SET logical_path = ?2, path_key = ?3,
                 revision = ?4, updated_at_us = ?5, author_device_id = ?6
             WHERE document_id = ?1",
            params![
                document_id.as_str(),
                logical_path,
                path_key(logical_path),
                u64_to_i64(revision, "document revision")?,
                updated_at_us,
                author_device_id,
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
            "DELETE FROM document_revision_asset WHERE document_id = ?1",
            params![document_id.as_str()],
        )
        .map_err(StoreError::storage)?;
    transaction
        .execute(
            "DELETE FROM document_projection_revision WHERE document_id = ?1",
            params![document_id.as_str()],
        )
        .map_err(StoreError::storage)?;
    transaction
        .execute(
            "DELETE FROM document_revision WHERE document_id = ?1",
            params![document_id.as_str()],
        )
        .map_err(StoreError::storage)?;
    transaction
        .execute(
            "INSERT INTO document_revision(
                 document_id, revision, logical_path, content_type, blob_sha256, operation,
                 created_at_us, author_device_id
             ) VALUES (
                 ?1, ?2, ?3,
                 (SELECT content_type FROM document WHERE document_id = ?1),
                 ?4, ?5, ?6, ?7
             )",
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
    for concept in &prepared.concepts {
        require_active_node(connection, concept.concept_id().as_str())?;
    }
    for asset_path in &prepared.asset_paths {
        require_active_association(
            connection,
            "managed_asset",
            "asset_path",
            asset_path,
            "asset",
        )?;
    }
    Ok(())
}

fn require_active_node(connection: &Connection, node_id: &str) -> Result<(), StoreError> {
    let active = connection
        .query_row(
            "SELECT active FROM node WHERE node_id = ?1",
            [node_id],
            |row| row.get::<_, bool>(0),
        )
        .optional()
        .map_err(StoreError::storage)?;
    if active == Some(true) {
        return Ok(());
    }
    let retired = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM retired_node WHERE node_id = ?1)",
            [node_id],
            |row| row.get::<_, bool>(0),
        )
        .map_err(StoreError::storage)?;
    if active == Some(false) || retired {
        Err(StoreError::InactiveReference { kind: "node" })
    } else {
        Err(StoreError::NotFound)
    }
}

fn validate_note_assets(
    connection: &Connection,
    prepared: &PreparedNote,
) -> Result<(), StoreError> {
    for asset_path in &prepared.asset_paths {
        require_active_association(
            connection,
            "managed_asset",
            "asset_path",
            asset_path,
            "asset",
        )?;
    }
    Ok(())
}

fn require_thema_associations_active(
    connection: &Connection,
    thema_id: &str,
) -> Result<(), StoreError> {
    let concepts = connection
        .prepare_cached("SELECT concept_id FROM thema_concept WHERE thema_id = ?1")
        .map_err(StoreError::storage)?
        .query_map([thema_id], |row| row.get::<_, String>(0))
        .map_err(StoreError::storage)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::storage)?;
    for concept_id in concepts {
        require_active_node(connection, &concept_id)?;
    }
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
        Err(StoreError::InactiveReference { kind: "asset" })
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

fn require_active_association(
    connection: &Connection,
    table: &str,
    column: &str,
    value: &str,
    kind: &'static str,
) -> Result<(), StoreError> {
    let query = format!("SELECT active FROM {table} WHERE {column} = ?1");
    let active = connection
        .query_row(&query, [value], |row| row.get::<_, bool>(0))
        .optional()
        .map_err(StoreError::storage)?;
    match active {
        Some(true) => Ok(()),
        Some(false) => Err(StoreError::InactiveReference { kind }),
        None => Err(StoreError::NotFound),
    }
}

fn thema_is_unchanged(
    connection: &Connection,
    prepared: &PreparedThema,
) -> Result<bool, StoreError> {
    connection
        .query_row(
            "SELECT content_sha256 = ?2
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
    author_device_id: Option<&str>,
) -> Result<(), StoreError> {
    let content = &prepared.content;
    transaction
        .execute(
            "INSERT INTO thema(
             thema_id, revision, kind, keimenon, hypothesis_json, answer_json,
             source_guid, parathema, review_image, content_sha256,
             created_at_us, updated_at_us, author_device_id
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11, ?12)
         ON CONFLICT(thema_id) DO UPDATE SET
             revision = excluded.revision, kind = excluded.kind,
             keimenon = excluded.keimenon, hypothesis_json = excluded.hypothesis_json,
             answer_json = excluded.answer_json, source_guid = excluded.source_guid,
             parathema = excluded.parathema, review_image = excluded.review_image,
             content_sha256 = excluded.content_sha256,
             updated_at_us = excluded.updated_at_us,
             author_device_id = excluded.author_device_id",
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
                author_device_id,
            ],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn replace_thema_associations(
    transaction: &Transaction<'_>,
    prepared: &PreparedThema,
) -> Result<(), StoreError> {
    for table in ["thema_tag", "thema_concept", "thema_asset"] {
        transaction
            .execute(
                &format!("DELETE FROM {table} WHERE thema_id = ?1"),
                [prepared.thema_id.as_str()],
            )
            .map_err(StoreError::storage)?;
    }
    insert_thema_tags(transaction, prepared)?;
    insert_thema_concepts(transaction, prepared)?;
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

fn insert_thema_concepts(
    transaction: &Transaction<'_>,
    prepared: &PreparedThema,
) -> Result<(), StoreError> {
    let mut insert = transaction
        .prepare_cached(
            "INSERT INTO thema_concept(thema_id, concept_id, relation) VALUES (?1, ?2, ?3)",
        )
        .map_err(StoreError::storage)?;
    for concept in &prepared.concepts {
        insert
            .execute(params![
                prepared.thema_id.as_str(),
                concept.concept_id().as_str(),
                concept.relation().as_str(),
            ])
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
            "INSERT OR IGNORE INTO review_state(
                 thema_id, state_revision, review_count, last_reviewed_at_us,
                 last_review_day, next_review_day, interval_days, total_successes,
                 total_failures, suspended, stability, difficulty
             ) VALUES (?1, 0, 0, NULL, NULL, ?2, NULL, 0, 0, 0, NULL, NULL)",
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
            "DELETE FROM thema_revision WHERE thema_id = ?1",
            params![record.thema_id],
        )
        .map_err(StoreError::storage)?;
    transaction
        .execute(
            "INSERT INTO thema_revision(
                 thema_id, revision, kind, keimenon, hypothesis_json, answer_json,
                 source_guid, parathema, review_image, tags_json, concepts_json,
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
                serde_json::to_string(&stored_concepts(&record.concepts))
                    .map_err(StoreError::storage)?,
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
                 revision, timezone, day_boundary_hour, scheduler_json,
                 operation, created_at_us, author_device_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                u64_to_i64(settings.revision, "settings revision")?,
                settings.timezone,
                i64::from(settings.day_boundary_hour),
                serde_json::to_string(&settings.scheduler).map_err(StoreError::storage)?,
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
        concepts: query_thema_concepts(connection, &id)?,
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

fn stored_concepts(concepts: &[ThemaConcept]) -> Vec<StoredThemaConcept> {
    concepts
        .iter()
        .map(|concept| StoredThemaConcept {
            node_id: concept.concept_id().to_string(),
            relation: concept.relation().as_str().to_owned(),
        })
        .collect()
}

#[allow(dead_code)]
fn parse_stored_concepts(thema_id: &str, value: &str) -> Result<Vec<ThemaConcept>, StoreError> {
    let concepts: Vec<StoredThemaConcept> =
        serde_json::from_str(value).map_err(StoreError::storage)?;
    materialize_thema_concepts(
        thema_id,
        concepts
            .into_iter()
            .map(|concept| (concept.node_id, concept.relation))
            .collect(),
    )
}

fn query_thema_concepts(
    connection: &Connection,
    thema_id: &str,
) -> Result<Vec<ThemaConcept>, StoreError> {
    let mut statement = connection
        .prepare(
            "SELECT concept_id, relation FROM thema_concept \
             WHERE thema_id = ?1 ORDER BY concept_id",
        )
        .map_err(StoreError::storage)?;
    let concepts = statement
        .query_map([thema_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(StoreError::storage)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::storage)?;
    materialize_thema_concepts(thema_id, concepts)
}

fn materialize_thema_concepts(
    thema_id: &str,
    concepts: Vec<(String, String)>,
) -> Result<Vec<ThemaConcept>, StoreError> {
    let thema_id = ThemaId::new(thema_id.to_owned()).map_err(|_| StoreError::CorruptData {
        field: "thema concept",
    })?;
    concepts
        .into_iter()
        .map(|(concept_id, relation)| {
            let concept_id = NodeId::new(concept_id).map_err(|_| StoreError::CorruptData {
                field: "thema concept",
            })?;
            let relation =
                ThemaConceptRelation::parse(&relation).ok_or(StoreError::CorruptData {
                    field: "thema concept",
                })?;
            Ok(ThemaConcept::new(thema_id.clone(), concept_id, relation))
        })
        .collect()
}

fn load_node(connection: &Connection, node_id: &str) -> Result<Option<NodeRecord>, StoreError> {
    let mut projections = NodeProjectionCache::unbounded();
    load_node_cached(connection, node_id, &mut projections)
}

struct NodeProjectionCache {
    locations: HashMap<(String, u64), Option<ProjectionNodeLocations>>,
    decoded_bytes: usize,
    max_decoded_bytes: Option<usize>,
}

impl NodeProjectionCache {
    fn unbounded() -> Self {
        Self {
            locations: HashMap::new(),
            decoded_bytes: 0,
            max_decoded_bytes: None,
        }
    }

    fn page() -> Self {
        Self {
            locations: HashMap::new(),
            decoded_bytes: 0,
            max_decoded_bytes: Some(MAX_DOCUMENT_PROJECTION_BYTES),
        }
    }

    fn reserve(&mut self, bytes: usize) -> bool {
        let Some(limit) = self.max_decoded_bytes else {
            return true;
        };
        let Some(total) = self.decoded_bytes.checked_add(bytes) else {
            return false;
        };
        if total > limit {
            return false;
        }
        self.decoded_bytes = total;
        true
    }
}

struct ProjectionNodeLocations {
    content_sha256: String,
    spans: HashMap<String, Option<SourceSpan>>,
}

fn load_node_records(
    connection: &Connection,
    ids: Vec<String>,
) -> Result<Vec<NodeRecord>, StoreError> {
    let mut projections = NodeProjectionCache::page();
    let mut records = Vec::with_capacity(ids.len());
    for id in ids {
        match load_node_cached(connection, &id, &mut projections) {
            Ok(Some(record)) => records.push(record),
            Ok(None) => return Err(StoreError::NotFound),
            Err(StoreError::NodePageProjectionLimit) => break,
            Err(error) => return Err(error),
        }
    }
    Ok(records)
}

fn load_node_cached(
    connection: &Connection,
    node_id: &str,
    projections: &mut NodeProjectionCache,
) -> Result<Option<NodeRecord>, StoreError> {
    if let Some(node) = load_current_node(connection, node_id, projections)? {
        return Ok(Some(node));
    }
    load_retired_node(connection, node_id, projections)
}

fn load_current_node(
    connection: &Connection,
    node_id: &str,
    projections: &mut NodeProjectionCache,
) -> Result<Option<NodeRecord>, StoreError> {
    let row = connection
        .query_row(
            "SELECT node.node_id, node.document_id, document.logical_path, node.title,
                node.title_path_json, node.level, node.active, document.revision,
                document.content_sha256
         FROM node JOIN document ON document.document_id = node.document_id
         WHERE node.node_id = ?1",
            [node_id],
            |row| {
                Ok(CurrentNodeRow {
                    id: row.get(0)?,
                    document_id: row.get(1)?,
                    path: row.get(2)?,
                    title: row.get(3)?,
                    title_path: row.get(4)?,
                    level: row.get(5)?,
                    active: row.get(6)?,
                    document_revision: row.get(7)?,
                    fallback_digest: row.get(8)?,
                })
            },
        )
        .optional()
        .map_err(StoreError::storage)?;
    row.map(|row| materialize_current_node(connection, row, projections))
        .transpose()
}

struct CurrentNodeRow {
    id: String,
    document_id: String,
    path: String,
    title: String,
    title_path: String,
    level: i64,
    active: bool,
    document_revision: i64,
    fallback_digest: String,
}

fn materialize_current_node(
    connection: &Connection,
    row: CurrentNodeRow,
    projections: &mut NodeProjectionCache,
) -> Result<NodeRecord, StoreError> {
    let head_revision = to_u64(row.document_revision, "document revision")?;
    let revision = if row.active {
        head_revision
    } else {
        head_revision
            .checked_sub(1)
            .ok_or(StoreError::CorruptData {
                field: "node document revision",
            })?
    };
    let location = load_node_location(
        connection,
        &row.document_id,
        revision,
        &row.id,
        &row.fallback_digest,
        row.active,
        projections,
    )?;
    Ok(NodeRecord {
        tags: query_node_tags(connection, &row.document_id, &row.id)?,
        node_id: row.id,
        document_id: row.document_id,
        logical_path: row.path,
        document_revision: revision,
        content_sha256: location.content_sha256,
        title: row.title,
        title_path: serde_json::from_str(&row.title_path).map_err(StoreError::storage)?,
        level: to_u32(row.level, "node level")?,
        start_byte: location.start_byte,
        end_byte: location.end_byte,
        active: row.active,
    })
}

fn load_retired_node(
    connection: &Connection,
    node_id: &str,
    projections: &mut NodeProjectionCache,
) -> Result<Option<NodeRecord>, StoreError> {
    let row = connection
        .query_row(
            "SELECT retired_node.node_id, retired_node.document_id,
                retired_node.logical_path, retired_node.title, retired_node.title_path_json,
                retired_node.tags_json, retired_node.level, retired_node.retired_at_revision,
                document.content_sha256
             FROM retired_node JOIN document USING(document_id)
             WHERE retired_node.node_id = ?1",
            [node_id],
            |row| {
                Ok(RetiredNodeRow {
                    id: row.get(0)?,
                    document_id: row.get(1)?,
                    path: row.get(2)?,
                    title: row.get(3)?,
                    title_path: row.get(4)?,
                    tags: row.get(5)?,
                    level: row.get(6)?,
                    retired_at_revision: row.get(7)?,
                    fallback_digest: row.get(8)?,
                })
            },
        )
        .optional()
        .map_err(StoreError::storage)?;
    row.map(|row| materialize_retired_node(connection, row, projections))
        .transpose()
}

struct RetiredNodeRow {
    id: String,
    document_id: String,
    path: String,
    title: String,
    title_path: String,
    tags: String,
    level: i64,
    retired_at_revision: i64,
    fallback_digest: String,
}

fn materialize_retired_node(
    connection: &Connection,
    row: RetiredNodeRow,
    projections: &mut NodeProjectionCache,
) -> Result<NodeRecord, StoreError> {
    let revision = to_u64(row.retired_at_revision, "retired revision")?
        .checked_sub(1)
        .ok_or(StoreError::CorruptData {
            field: "node document revision",
        })?;
    let location = load_node_location(
        connection,
        &row.document_id,
        revision,
        &row.id,
        &row.fallback_digest,
        false,
        projections,
    )?;
    Ok(NodeRecord {
        node_id: row.id,
        document_id: row.document_id,
        logical_path: row.path,
        document_revision: revision,
        content_sha256: location.content_sha256,
        title: row.title,
        title_path: serde_json::from_str(&row.title_path).map_err(StoreError::storage)?,
        tags: serde_json::from_str(&row.tags).map_err(StoreError::storage)?,
        level: to_u32(row.level, "node level")?,
        start_byte: location.start_byte,
        end_byte: location.end_byte,
        active: false,
    })
}

struct NodeLocation {
    content_sha256: String,
    start_byte: Option<usize>,
    end_byte: Option<usize>,
}

fn load_node_location(
    connection: &Connection,
    document_id: &str,
    revision: u64,
    node_id: &str,
    fallback_digest: &str,
    require_projection: bool,
    projections: &mut NodeProjectionCache,
) -> Result<NodeLocation, StoreError> {
    let key = (document_id.to_owned(), revision);
    if !projections.locations.contains_key(&key) {
        let snapshot = load_projection_snapshot(connection, document_id, revision)?;
        if snapshot
            .as_ref()
            .is_some_and(|(size, _)| !projections.reserve(*size))
        {
            return Err(StoreError::NodePageProjectionLimit);
        }
        let locations = snapshot
            .map(|(_, json)| serde_json::from_str(&json).map_err(StoreError::storage))
            .transpose()?
            .map(projection_node_locations)
            .transpose()?;
        projections.locations.insert(key.clone(), locations);
    }
    let Some(locations) = projections.locations.get(&key).and_then(Option::as_ref) else {
        if require_projection {
            return Err(StoreError::CorruptData {
                field: "document projection",
            });
        }
        return Ok(NodeLocation {
            content_sha256: fallback_digest.to_owned(),
            start_byte: None,
            end_byte: None,
        });
    };
    let span = locations
        .spans
        .get(node_id)
        .copied()
        .ok_or(StoreError::CorruptData {
            field: "node projection history",
        })?;
    Ok(NodeLocation {
        content_sha256: locations.content_sha256.clone(),
        start_byte: span.map(|value| value.start()),
        end_byte: span.map(|value| value.end()),
    })
}

fn projection_node_locations(
    projection: DocumentProjectionSnapshot,
) -> Result<ProjectionNodeLocations, StoreError> {
    if !crate::is_canonical_sha256(&projection.content_sha256) {
        return Err(StoreError::CorruptData {
            field: "document content digest",
        });
    }
    let mut spans = HashMap::with_capacity(projection.nodes.len());
    for node in projection.nodes {
        let span = projection_node_span(node.start_byte, node.end_byte)?;
        if spans.insert(node.node_id, span).is_some() {
            return Err(StoreError::CorruptData {
                field: "node projection history",
            });
        }
    }
    Ok(ProjectionNodeLocations {
        content_sha256: projection.content_sha256,
        spans,
    })
}

fn load_projection_snapshot(
    connection: &Connection,
    document_id: &str,
    revision: u64,
) -> Result<Option<(usize, String)>, StoreError> {
    let mut statement = connection
        .prepare(
            "SELECT length(CAST(projection_json AS BLOB)), projection_json
             FROM document_projection AS projection
             JOIN document USING(document_id)
             WHERE projection.document_id = ?1 AND document.revision = ?2
               AND document.active = 1",
        )
        .map_err(StoreError::storage)?;
    let mut rows = statement
        .query(params![
            document_id,
            u64_to_i64(revision, "document revision")?
        ])
        .map_err(StoreError::storage)?;
    let Some(row) = rows.next().map_err(StoreError::storage)? else {
        return Ok(None);
    };
    let size = to_usize(
        row.get::<_, i64>(0).map_err(StoreError::storage)?,
        "document projection size",
    )?;
    if size > MAX_DOCUMENT_PROJECTION_BYTES {
        return Err(StoreError::CorruptData {
            field: "document projection size",
        });
    }
    let json = row.get::<_, String>(1).map_err(StoreError::storage)?;
    Ok(Some((size, json)))
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

fn initialize_schema(connection: &Connection) -> Result<(), StoreError> {
    let found = schema_version(connection)?;
    if found == 0 && user_schema_is_empty(connection)? {
        connection
            .execute_batch(CURRENT_SCHEMA)
            .map_err(StoreError::storage)?;
        require_application_id(connection)?;
        return require_current_schema_layout(connection);
    }
    match found {
        SCHEMA_VERSION => {
            require_application_id(connection)?;
            require_current_schema_layout(connection)
        }
        _ => Err(StoreError::UnsupportedSchemaVersion {
            found,
            supported: SCHEMA_VERSION,
        }),
    }
}

fn require_current_schema_layout(connection: &Connection) -> Result<(), StoreError> {
    require_schema_layout(connection, CURRENT_SCHEMA)
}

fn require_schema_layout(connection: &Connection, schema: &str) -> Result<(), StoreError> {
    let expected = Connection::open_in_memory().map_err(StoreError::storage)?;
    expected
        .execute_batch(schema)
        .map_err(StoreError::storage)?;
    if schema_objects(connection)? == schema_objects(&expected)? {
        Ok(())
    } else {
        Err(StoreError::UnsupportedSchemaLayout)
    }
}

fn schema_objects(
    connection: &Connection,
) -> Result<Vec<(String, String, String, String)>, StoreError> {
    let mut statement = connection
        .prepare(
            "SELECT type, name, tbl_name, sql FROM sqlite_schema \
             WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
        )
        .map_err(StoreError::storage)?;
    statement
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .map_err(StoreError::storage)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::storage)
}

fn require_application_id(connection: &Connection) -> Result<(), StoreError> {
    let found = application_id(connection)?;
    if found == APPLICATION_ID {
        Ok(())
    } else {
        Err(StoreError::UnsupportedApplicationId {
            found,
            expected: APPLICATION_ID,
        })
    }
}

fn user_schema_is_empty(connection: &Connection) -> Result<bool, StoreError> {
    connection
        .query_row(
            "SELECT NOT EXISTS(
                 SELECT 1 FROM sqlite_schema
                 WHERE name NOT LIKE 'sqlite_%'
             )",
            [],
            |row| row.get(0),
        )
        .map_err(StoreError::storage)
}

fn schema_version(connection: &Connection) -> Result<u32, StoreError> {
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(StoreError::storage)?;
    u32::try_from(version).map_err(|_| StoreError::CorruptData {
        field: "schema version",
    })
}

fn application_id(connection: &Connection) -> Result<u32, StoreError> {
    connection
        .pragma_query_value(None, "application_id", |row| row.get(0))
        .map_err(StoreError::storage)
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

fn load_thema_catalog_record(
    connection: &Connection,
    thema_id: &str,
) -> Result<ThemaCatalogRecord, StoreError> {
    type CatalogRow = (i64, i64, i64, String, String, String, String, bool, bool);
    let row = connection
        .query_row(
            "SELECT thema.revision, review_state.state_revision, review_state.review_count,
                    thema.kind,
                    thema.keimenon, thema.hypothesis_json, thema.answer_json,
                    thema.archived_at_us IS NOT NULL, review_state.suspended
             FROM thema JOIN review_state USING(thema_id)
             WHERE thema.thema_id = ?1",
            [thema_id],
            |row| -> rusqlite::Result<CatalogRow> {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                ))
            },
        )
        .map_err(StoreError::storage)?;
    Ok(ThemaCatalogRecord {
        tags: query_strings(
            connection,
            "SELECT tag FROM thema_tag WHERE thema_id = ?1 ORDER BY tag",
            thema_id,
        )?,
        thema_id: thema_id.to_owned(),
        thema_revision: to_u64(row.0, "thema revision")?,
        state_revision: to_u64(row.1, "state revision")?,
        review_count: to_u64(row.2, "review count")?,
        kind: ThemaKind::parse(&row.3).map_err(StoreError::storage)?,
        keimenon: row.4,
        hypothesis: serde_json::from_str(&row.5).map_err(StoreError::storage)?,
        answer: serde_json::from_str(&row.6).map_err(StoreError::storage)?,
        archived: row.7,
        suspended: row.8,
    })
}

fn load_due_legacy_themata(
    connection: &Connection,
    day: u32,
    limit: u16,
    after: Option<(bool, u32, String)>,
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
              AND state.thema_id > ?4)) \
     ORDER BY (state.review_count > 0), state.next_review_day, \
              state.thema_id \
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

fn optional_u64(value: Option<i64>, field: &'static str) -> Result<Option<u64>, StoreError> {
    value.map(|value| to_u64(value, field)).transpose()
}

fn to_u64(value: i64, field: &'static str) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| StoreError::CorruptData { field })
}

fn to_u32(value: i64, field: &'static str) -> Result<u32, StoreError> {
    u32::try_from(value).map_err(|_| StoreError::CorruptData { field })
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
    content_type: &str,
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
    insert_document(transaction, document_id, projection, content_type, revision)?;
    #[cfg(test)]
    if fail_point == FailPoint::AfterDocument {
        return Err(StoreError::storage(std::io::Error::other(
            "injected write failure",
        )));
    }
    let _ = fail_point;
    insert_nodes(transaction, document_id, projection.nodes())?;
    insert_occurrences(transaction, document_id, projection.link_occurrences())?;
    insert_edges(transaction, document_id, projection.semantic_edges())?;
    let projection_json = serde_json::to_string(&DocumentProjectionSnapshot::from(projection))
        .map_err(StoreError::storage)?;
    transaction
        .execute(
            "INSERT INTO document_projection(document_id, projection_json) VALUES (?1, ?2)
             ON CONFLICT(document_id) DO UPDATE SET projection_json = excluded.projection_json",
            params![document_id.as_str(), projection_json],
        )
        .map(|_| ())
        .map_err(StoreError::storage)
}

fn insert_document(
    transaction: &Transaction<'_>,
    document_id: &DocumentId,
    projection: &DocumentProjection,
    content_type: &str,
    revision: u64,
) -> Result<(), StoreError> {
    let revision =
        i64::try_from(revision).map_err(|_| StoreError::CorruptData { field: "revision" })?;
    let file_tags = serde_json::to_string(projection.file_tags()).map_err(StoreError::storage)?;
    transaction
        .execute(
            "INSERT INTO document (\
             document_id, revision, title, file_node_id, file_tags_json, \
             content_sha256, content_type\
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(document_id) DO UPDATE SET
                 revision = excluded.revision,
                 title = excluded.title,
                 file_node_id = excluded.file_node_id,
                 file_tags_json = excluded.file_tags_json,
                 content_sha256 = excluded.content_sha256,
                 content_type = excluded.content_type",
            params![
                document_id.as_str(),
                revision,
                projection.title(),
                projection.file_node_id().map(NodeId::as_str),
                file_tags,
                projection.content_sha256(),
                content_type,
            ],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn check_path_available(
    connection: &Connection,
    prepared: &PreparedNote,
) -> Result<(), StoreError> {
    let owner = connection
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
        Err(StoreError::PathConflict)
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
    match note_write_decision(transaction, prepared, expected_revision, allow_noop)? {
        NoteWriteDecision::Unchanged(current) => Ok(current),
        NoteWriteDecision::Write(current) => write_note_revision(
            transaction,
            prepared,
            blob,
            current,
            created_at_us,
            author_device_id,
        ),
    }
}

pub(super) enum NoteWriteDecision {
    Unchanged(StoredDocument),
    Write(Option<StoredDocument>),
}

pub(super) fn note_write_decision(
    connection: &Connection,
    prepared: &PreparedNote,
    expected_revision: Option<u64>,
    allow_noop: bool,
) -> Result<NoteWriteDecision, StoreError> {
    let current = load_document(connection, &prepared.document_id)?;
    let _ = expected_revision;
    validate_note_assets(connection, prepared)?;
    let current_path = document_path(connection, &prepared.document_id)?;
    require_unchanged_path(prepared, current_path.as_deref())?;
    check_path_available(connection, prepared)?;
    check_node_ids_available(connection, prepared)?;
    if allow_noop && note_is_unchanged(connection, prepared, current.as_ref(), &current_path)? {
        return current
            .map(NoteWriteDecision::Unchanged)
            .ok_or(StoreError::CorruptData { field: "document" });
    }
    Ok(NoteWriteDecision::Write(current))
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

fn check_node_ids_available(
    connection: &Connection,
    prepared: &PreparedNote,
) -> Result<(), StoreError> {
    let mut conflict = connection
        .prepare_cached(
            "SELECT EXISTS(
               SELECT 1 FROM node WHERE node_id = ?1 AND document_id <> ?2
               UNION ALL
               SELECT 1 FROM retired_node WHERE node_id = ?1 AND document_id <> ?2
             )",
        )
        .map_err(StoreError::storage)?;
    for node in prepared.projection.nodes() {
        let unavailable = conflict
            .query_row([node.id().as_str(), prepared.document_id.as_str()], |row| {
                row.get::<_, bool>(0)
            })
            .map_err(StoreError::storage)?;
        if unavailable {
            return Err(StoreError::NodeIdConflict {
                node_id: node.id().as_str().to_owned(),
            });
        }
    }
    Ok(())
}

fn note_is_unchanged(
    connection: &Connection,
    prepared: &PreparedNote,
    current: Option<&StoredDocument>,
    current_path: &Option<String>,
) -> Result<bool, StoreError> {
    let head = load_document_head(connection, &prepared.document_id)?;
    let blob_matches = head.is_some_and(|head| {
        head.active
            && head.blob_sha256.as_deref() == Some(prepared.blob_sha256.as_str())
            && head.content_type == prepared.content_type
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
        &prepared.content_type,
        revision,
        FailPoint::None,
    )?;
    update_note_head(
        transaction,
        prepared,
        blob,
        revision,
        created_at_us,
        author_device_id,
    )?;
    replace_document_assets(transaction, prepared)?;
    record_note_history(
        transaction,
        prepared,
        revision,
        current.is_some(),
        created_at_us,
        author_device_id,
    )?;
    delete_unreferenced_blobs(transaction)?;
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

pub(super) fn insert_blob(
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

pub(super) fn delete_unreferenced_blobs(transaction: &Transaction<'_>) -> Result<(), StoreError> {
    transaction
        .execute(
            "DELETE FROM blob
             WHERE content_sha256 NOT IN (
                 SELECT head_blob_sha256 FROM document WHERE head_blob_sha256 IS NOT NULL
                 UNION
                 SELECT blob_sha256 FROM document_revision WHERE blob_sha256 IS NOT NULL
                 UNION
                 SELECT blob_sha256 FROM managed_asset
                 UNION
                 SELECT blob_sha256 FROM asset_revision WHERE blob_sha256 IS NOT NULL
                 UNION
                 SELECT content_sha256 FROM review_pack_asset
                 UNION
                 SELECT content_sha256 FROM staged_blob
                 UNION
                 SELECT content_sha256 FROM offline_asset_evidence
             )",
            [],
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
    author_device_id: Option<&str>,
) -> Result<(), StoreError> {
    transaction
        .execute(
            "UPDATE document SET logical_path = ?2, path_key = ?3,
                 head_blob_sha256 = ?4, content_type = ?7,
                 active = 1, updated_at_us = ?5,
                 revision = ?6, author_device_id = ?8 WHERE document_id = ?1",
            params![
                prepared.document_id.as_str(),
                prepared.logical_path,
                prepared.path_key,
                blob.sha256,
                updated_at_us,
                u64_to_i64(revision, "document revision")?,
                prepared.content_type,
                author_device_id,
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
            "DELETE FROM document_revision_asset WHERE document_id = ?1",
            params![prepared.document_id.as_str()],
        )
        .map_err(StoreError::storage)?;
    transaction
        .execute(
            "DELETE FROM document_projection_revision WHERE document_id = ?1",
            params![prepared.document_id.as_str()],
        )
        .map_err(StoreError::storage)?;
    transaction
        .execute(
            "DELETE FROM document_revision WHERE document_id = ?1",
            params![prepared.document_id.as_str()],
        )
        .map_err(StoreError::storage)?;
    transaction
        .execute(
            "INSERT INTO document_revision(
                 document_id, revision, logical_path, content_type, blob_sha256, operation,
                 created_at_us, author_device_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                prepared.document_id.as_str(),
                u64_to_i64(revision, "document revision")?,
                prepared.logical_path,
                prepared.content_type,
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
                     document_id = excluded.document_id,
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
            "INSERT INTO node_identity(node_id) VALUES (?1) ON CONFLICT DO NOTHING",
            [node.id().as_str()],
        )
        .map_err(StoreError::storage)?;
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
             content_sha256, active \
             FROM document WHERE document_id = ?1",
            [document_id.as_str()],
            |row| {
                Ok(DocumentRow {
                    revision: row.get(0)?,
                    title: row.get(1)?,
                    file_node_id: row.get(2)?,
                    file_tags_json: row.get(3)?,
                    content_sha256: row.get(4)?,
                    active: row.get(5)?,
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
    content_sha256: String,
    active: bool,
}

fn materialize_document(
    connection: &Connection,
    document_id: &DocumentId,
    row: DocumentRow,
) -> Result<StoredDocument, StoreError> {
    let revision =
        u64::try_from(row.revision).map_err(|_| StoreError::CorruptData { field: "revision" })?;
    let node_spans = load_projection_node_spans(connection, document_id, revision)?;
    if row.active && node_spans.is_none() {
        return Err(StoreError::CorruptData {
            field: "document projection",
        });
    }
    let nodes = load_nodes(connection, document_id, node_spans.as_ref())?;
    let link_occurrences = load_occurrences(connection, document_id)?;
    let stored_edges = load_edges(connection, document_id)?;
    let projection = DocumentProjection::new(DocumentProjectionInput {
        title: row.title,
        file_node_id: row.file_node_id.map(node_id).transpose()?,
        file_tags: from_json(&row.file_tags_json, "file tags")?,
        nodes,
        link_occurrences,
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

fn load_projection_node_spans(
    connection: &Connection,
    document_id: &DocumentId,
    revision: u64,
) -> Result<Option<HashMap<String, Option<SourceSpan>>>, StoreError> {
    load_projection_snapshot(connection, document_id.as_str(), revision)?
        .map(|(_, json)| {
            serde_json::from_str::<DocumentProjectionSnapshot>(&json).map_err(StoreError::storage)
        })
        .transpose()?
        .map(projection_node_locations)
        .transpose()
        .map(|locations| locations.map(|locations| locations.spans))
}

fn projection_node_span(
    start_byte: Option<usize>,
    end_byte: Option<usize>,
) -> Result<Option<SourceSpan>, StoreError> {
    match (start_byte, end_byte) {
        (Some(start), Some(end)) if end <= MAX_DOCUMENT_SOURCE_BYTES => {
            SourceSpan::from_offsets(start, end)
                .map(Some)
                .map_err(|_| StoreError::CorruptData {
                    field: "node source span",
                })
        }
        (None, None) => Ok(None),
        _ => Err(StoreError::CorruptData {
            field: "node source span",
        }),
    }
}

fn load_nodes(
    connection: &Connection,
    document_id: &DocumentId,
    node_spans: Option<&HashMap<String, Option<SourceSpan>>>,
) -> Result<Vec<Node>, StoreError> {
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
    rows.map(|row| {
        materialize_node(
            connection,
            document_id,
            row.map_err(StoreError::storage)?,
            node_spans,
        )
    })
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
    node_spans: Option<&HashMap<String, Option<SourceSpan>>>,
) -> Result<Node, StoreError> {
    let source_span = node_spans
        .map(|spans| {
            spans.get(&row.id).copied().ok_or(StoreError::CorruptData {
                field: "node projection history",
            })
        })
        .transpose()?
        .flatten();
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
    match source_span {
        Some(span) => Node::new(id, row.title, title_path, tags, parent, level)
            .map(|node| node.with_source_span(span)),
        None => Node::new(id, row.title, title_path, tags, parent, level),
    }
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

fn backup_database_with_step(
    source: &Path,
    destination: &Path,
    retry_budget: Duration,
    cancelled: &AtomicBool,
    mut after_step: impl FnMut(StepResult),
) -> Result<(), StoreError> {
    let source = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(StoreError::storage)?;
    source
        .busy_timeout(Duration::ZERO)
        .map_err(StoreError::storage)?;
    let mut destination = Connection::open(destination).map_err(StoreError::storage)?;
    let backup = Backup::new(&source, &mut destination).map_err(StoreError::storage)?;
    let started = Instant::now();
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(StoreError::BackupCancelled);
        }
        let result = backup.step(100).map_err(StoreError::storage)?;
        after_step(result);
        if result == StepResult::Done {
            return Ok(());
        }
        let elapsed = started.elapsed();
        if elapsed >= retry_budget {
            return Err(StoreError::BackupDeadlineExceeded);
        }
        std::thread::sleep(BACKUP_RETRY_DELAY.min(retry_budget - elapsed));
    }
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
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        thread,
        time::Duration,
    };

    use rusqlite::{Connection, backup::StepResult};
    use sha2::Digest as _;
    use tempfile::tempdir;

    use super::{
        APPLICATION_ID, CURRENT_SCHEMA, DocumentAssetAssociation, FailPoint, SCHEMA_VERSION,
        SqliteStore, StoreError, application_id, backup_database_with_step, initialize_schema,
        replace_rows, retire_removed_nodes, schema_objects, schema_version,
    };
    use crate::application::{
        DeviceDesiredStateMutation, DeviceMutationProjection, DeviceMutationResult,
        MAX_DOCUMENT_PROJECTION_BYTES, NodeThemaAssociation, PreparedNote, ReplicaProjection,
        ReviewSource, ReviewSourceKind, SettingsInput, generate_device, prepare_asset,
        prepare_device_desired_state_mutation, prepare_note, prepare_provisioned_device,
        prepare_thema, thema_content,
    };
    use crate::blob::BlobMeta;
    use crate::domain::{
        DocumentId, DocumentProjection, DocumentProjectionInput, LinkOccurrence, Node, NodeId,
        SourceSpan, ThemaConceptRelation, ThemaId, TitlePath,
    };

    #[test]
    fn device_provisioning_exact_retry_reuses_record_and_mismatch_conflicts() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let device = generate_device("phone".into(), vec!["read".into()]).unwrap();
        let other = generate_device("other".into(), vec!["read".into()]).unwrap();
        let first = store.create_device(&device, 1).unwrap();
        let replay = store.create_device(&device, 2).unwrap();
        assert_eq!(replay, first);
        assert_eq!(
            store.devices().unwrap().as_slice(),
            std::slice::from_ref(&first)
        );
        assert!(
            store
                .authenticate(&device.token_digest, 2)
                .unwrap()
                .is_some()
        );
        let after_authentication = store.devices().unwrap();

        for (name, token, scopes) in [
            ("other", device.token.clone(), vec!["read".into()]),
            ("phone", other.token.clone(), vec!["read".into()]),
            ("phone", device.token.clone(), vec!["review".into()]),
        ] {
            let mismatch =
                prepare_provisioned_device(device.device_id.clone(), name.into(), token, scopes)
                    .unwrap();
            assert!(matches!(
                store.create_device(&mismatch, 3),
                Err(StoreError::DeviceProvisioningConflict)
            ));
        }
        assert_eq!(store.devices().unwrap(), after_authentication);
        assert!(
            store
                .authenticate(&other.token_digest, 3)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn backup_retries_while_source_writer_holds_lock() {
        let directory = tempdir().unwrap();
        let source_path = directory.path().join("source.sqlite3");
        let destination_path = directory.path().join("backup.sqlite3");
        let writer = Connection::open(&source_path).unwrap();
        writer
            .execute_batch(
                "CREATE TABLE content (value BLOB NOT NULL);
                 WITH RECURSIVE rows(n) AS (
                     SELECT 1 UNION ALL SELECT n + 1 FROM rows WHERE n < 255
                 )
                 INSERT INTO content SELECT zeroblob(4096) FROM rows;
                 BEGIN EXCLUSIVE;
                 INSERT INTO content VALUES (zeroblob(4096));",
            )
            .unwrap();

        let (busy_sender, busy_receiver) = mpsc::sync_channel(0);
        let (release_sender, release_receiver) = mpsc::sync_channel(0);
        let backup = thread::spawn(move || {
            let mut saw_busy = false;
            let cancelled = AtomicBool::new(false);
            let result = backup_database_with_step(
                &source_path,
                &destination_path,
                Duration::from_secs(1),
                &cancelled,
                |result| {
                    if matches!(result, StepResult::Busy | StepResult::Locked) && !saw_busy {
                        saw_busy = true;
                        busy_sender.send(()).unwrap();
                        release_receiver.recv().unwrap();
                    }
                },
            );
            (result, destination_path)
        });

        busy_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("backup must observe writer contention");
        writer.execute_batch("COMMIT;").unwrap();
        release_sender.send(()).unwrap();

        let (result, destination_path) = backup.join().unwrap();
        result.unwrap();
        let destination = Connection::open(destination_path).unwrap();
        let rows: u64 = destination
            .query_row("SELECT COUNT(*) FROM content", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 256);
        let integrity: String = destination
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(integrity, "ok");
    }

    #[test]
    fn backup_stops_after_persistent_contention() {
        let directory = tempdir().unwrap();
        let source_path = directory.path().join("source.sqlite3");
        let destination_path = directory.path().join("backup.sqlite3");
        let writer = Connection::open(&source_path).unwrap();
        writer
            .execute_batch(
                "CREATE TABLE content (value TEXT NOT NULL);
                 BEGIN EXCLUSIVE;
                 INSERT INTO content VALUES ('locked');",
            )
            .unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let backup_cancelled = cancelled.clone();

        let result = thread::spawn(move || {
            backup_database_with_step(
                &source_path,
                &destination_path,
                Duration::from_millis(50),
                &backup_cancelled,
                |_| {},
            )
        })
        .join()
        .unwrap();

        assert!(matches!(result, Err(StoreError::BackupDeadlineExceeded)));
        writer.execute_batch("ROLLBACK;").unwrap();
    }

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
            content_sha256: format!("{:x}", sha2::Sha256::digest(digest.as_bytes())),
        })
        .unwrap()
    }

    fn concept_projection() -> DocumentProjection {
        let primary = NodeId::new("primary").unwrap();
        DocumentProjection::new(DocumentProjectionInput {
            title: "Concepts".into(),
            file_node_id: Some(primary.clone()),
            file_tags: vec![],
            nodes: [
                (primary, "Primary"),
                (NodeId::new("supporting").unwrap(), "Supporting"),
                (NodeId::new("reference").unwrap(), "Reference"),
            ]
            .into_iter()
            .map(|(node_id, title)| {
                Node::new(
                    node_id,
                    title.into(),
                    TitlePath::new(vec![title.into()]).unwrap(),
                    vec![],
                    None,
                    0,
                )
                .unwrap()
            })
            .collect(),
            link_occurrences: vec![],
            content_sha256: format!("{:x}", sha2::Sha256::digest(b"concept-digest")),
        })
        .unwrap()
    }

    fn thema(question: &str, revision: Option<u64>) -> crate::application::PreparedThema {
        thema_named("thema", question, revision)
    }

    fn thema_named(
        thema_id: &str,
        question: &str,
        revision: Option<u64>,
    ) -> crate::application::PreparedThema {
        prepare_thema(
            Some(thema_id.into()),
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
                    desired_retention: settings.scheduler.desired_retention,
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
    fn thema_concepts_require_known_nodes_and_typed_relations() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        store
            .connection
            .execute("INSERT INTO thema (thema_id) VALUES ('thema')", [])
            .unwrap();
        let missing = store
            .connection
            .execute(
                "INSERT INTO thema_concept (thema_id, concept_id, relation) \
                 VALUES ('thema', 'missing', 'primary')",
                [],
            )
            .unwrap_err();
        assert!(
            missing
                .to_string()
                .contains("FOREIGN KEY constraint failed")
        );

        store
            .replace_document(
                &DocumentId::new("document").unwrap(),
                &projection("Document", "digest"),
                None,
            )
            .unwrap();
        let relation = store
            .connection
            .execute(
                "INSERT INTO thema_concept (thema_id, concept_id, relation) \
                 VALUES ('thema', 'root', 'related')",
                [],
            )
            .unwrap_err();
        assert!(relation.to_string().contains("CHECK constraint failed"));
    }

    #[test]
    fn node_themata_pages_relations_and_rejects_stale_cursors() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let document_id = DocumentId::new("concept-document").unwrap();
        store
            .replace_document(&document_id, &concept_projection(), None)
            .unwrap();
        let expected = (0..1_201)
            .map(|index| format!("thema-{index:04}"))
            .collect::<Vec<_>>();
        for (index, thema_id) in expected.iter().enumerate() {
            let relation = match index % 3 {
                0 => "primary",
                1 => "supporting",
                _ => "reference",
            };
            store
                .connection
                .execute("INSERT INTO thema (thema_id) VALUES (?1)", [thema_id])
                .unwrap();
            store
                .connection
                .execute(
                    "INSERT INTO thema_concept (thema_id, concept_id, relation)
                     VALUES (?1, 'primary', ?2)",
                    rusqlite::params![thema_id, relation],
                )
                .unwrap();
        }

        let mut associations = Vec::new();
        let mut after = None;
        loop {
            let page = store.node_themata("primary", 97, after.as_deref()).unwrap();
            assert!(page.len() <= 97);
            if page.is_empty() {
                break;
            }
            after = page.last().map(|item| item.thema_id.clone());
            associations.extend(page);
        }
        assert_eq!(
            associations
                .iter()
                .map(|item| item.thema_id.clone())
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(associations[0].relation, ThemaConceptRelation::Primary);
        assert_eq!(associations[1].relation, ThemaConceptRelation::Supporting);
        assert_eq!(associations[2].relation, ThemaConceptRelation::Reference);

        let stale = associations[500].thema_id.clone();
        store
            .connection
            .execute("DELETE FROM thema WHERE thema_id = ?1", [&stale])
            .unwrap();
        assert!(matches!(
            store.node_themata("primary", 97, Some(&stale)),
            Err(StoreError::InvalidCursor)
        ));
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
    fn cross_document_node_id_collision_is_actionable_and_rolls_back() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let original = prepare_note(
            Some("original".into()),
            "original.org".into(),
            ":PROPERTIES:\n:ID: shared\n:END:\n#+title: Original\n".into(),
            None,
        )
        .unwrap();
        let original_blob = BlobMeta {
            sha256: original.blob_sha256.clone(),
            size_bytes: original.source.len() as u64,
            media_type: crate::parser::ORG_CONTENT_TYPE.into(),
        };
        store
            .replace_note(&original, &original_blob, 1, None)
            .unwrap();
        let retired = prepare_note(
            Some("original".into()),
            "original.org".into(),
            "#+title: Original\n".into(),
            Some(1),
        )
        .unwrap();
        let retired_blob = BlobMeta {
            sha256: retired.blob_sha256.clone(),
            size_bytes: retired.source.len() as u64,
            media_type: crate::parser::ORG_CONTENT_TYPE.into(),
        };
        store
            .replace_note(&retired, &retired_blob, 2, None)
            .unwrap();
        let conflicting = prepare_note(
            Some("conflicting".into()),
            "conflicting.org".into(),
            ":PROPERTIES:\n:ID: shared\n:END:\n#+title: Conflicting\n".into(),
            None,
        )
        .unwrap();
        let conflicting_blob = BlobMeta {
            sha256: conflicting.blob_sha256.clone(),
            size_bytes: conflicting.source.len() as u64,
            media_type: crate::parser::ORG_CONTENT_TYPE.into(),
        };

        let error = store
            .replace_note(&conflicting, &conflicting_blob, 3, None)
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("node ID shared belongs to another document")
        );
        assert_eq!(
            store
                .get_document(&DocumentId::new("original").unwrap())
                .unwrap()
                .unwrap()
                .revision(),
            2
        );
        assert!(
            store
                .get_document(&DocumentId::new("conflicting").unwrap())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn retired_thema_concept_is_distinct_from_missing_thema() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let original = prepare_note(
            Some("document".into()),
            "note.org".into(),
            concat!(
                ":PROPERTIES:\n:ID: root\n:END:\n#+title: Note\n",
                "* Child\n:PROPERTIES:\n:ID: child\n:END:\n"
            )
            .into(),
            None,
        )
        .unwrap();
        let original_blob = BlobMeta {
            sha256: original.blob_sha256.clone(),
            size_bytes: original.source.len() as u64,
            media_type: crate::parser::ORG_CONTENT_TYPE.into(),
        };
        store
            .replace_note(&original, &original_blob, 1, None)
            .unwrap();
        let associated = prepare_thema(
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
        let stored_thema = store.put_thema(&associated, 20260715, 2, None).unwrap();
        let replacement = prepare_note(
            Some("document".into()),
            "note.org".into(),
            ":PROPERTIES:\n:ID: root\n:END:\n#+title: Note\n".into(),
            Some(1),
        )
        .unwrap();
        let replacement_blob = BlobMeta {
            sha256: replacement.blob_sha256.clone(),
            size_bytes: replacement.source.len() as u64,
            media_type: crate::parser::ORG_CONTENT_TYPE.into(),
        };
        store
            .replace_note(&replacement, &replacement_blob, 3, None)
            .unwrap();

        assert_eq!(
            store.get_thema("thema").unwrap().as_ref(),
            Some(&stored_thema)
        );
        let content_changes = store.latest_content_cursor().unwrap();
        let history: u64 = store
            .connection
            .query_row("SELECT COUNT(*) FROM thema_revision", [], |row| row.get(0))
            .unwrap();
        let retired_error = store
            .put_thema(
                &prepare_thema(
                    Some("thema".into()),
                    associated.content.clone(),
                    vec![],
                    vec![("child".into(), ThemaConceptRelation::Primary)],
                    vec![],
                    Some(1),
                )
                .unwrap(),
                20260715,
                4,
                None,
            )
            .unwrap_err();
        assert!(matches!(
            retired_error,
            StoreError::InactiveReference { kind: "node" }
        ));
        let missing_error = store
            .put_thema(
                &prepare_thema(
                    Some("thema".into()),
                    associated.content.clone(),
                    vec![],
                    vec![("missing".into(), ThemaConceptRelation::Primary)],
                    vec![],
                    Some(1),
                )
                .unwrap(),
                20260715,
                5,
                None,
            )
            .unwrap_err();
        assert!(matches!(missing_error, StoreError::NotFound));
        assert_eq!(
            store.get_thema("thema").unwrap().as_ref(),
            Some(&stored_thema)
        );
        assert_eq!(store.latest_content_cursor().unwrap(), content_changes);
        assert_eq!(
            store
                .connection
                .query_row::<u64, _, _>("SELECT COUNT(*) FROM thema_revision", [], |row| {
                    row.get(0)
                })
                .unwrap(),
            history
        );

        store
            .put_thema(
                &prepare_thema(
                    Some("thema".into()),
                    associated.content,
                    vec![],
                    vec![("root".into(), ThemaConceptRelation::Primary)],
                    vec![],
                    Some(1),
                )
                .unwrap(),
                20260715,
                6,
                None,
            )
            .unwrap();
    }

    #[test]
    fn inactive_thema_asset_has_one_error_for_put_and_unarchive() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let asset_digest = register_asset(&mut store, b"inactive association");
        let asset_path = format!("assets/{asset_digest}/asset.png");
        let prepared = prepare_thema(
            Some("thema".into()),
            thema_content(
                "basic",
                "Question".into(),
                vec![],
                vec!["Answer".into()],
                None,
                None,
                Some(asset_path.clone()),
            )
            .unwrap(),
            vec![],
            vec![],
            vec![asset_path.clone()],
            None,
        )
        .unwrap();
        store.put_thema(&prepared, 20260715, 2, None).unwrap();
        store.set_thema_archived("thema", 1, true, 3, None).unwrap();
        store.tombstone_asset(&asset_path, 1, 4, None).unwrap();

        let put_error = store
            .put_thema(
                &prepare_thema(
                    Some("thema".into()),
                    prepared.content,
                    vec![],
                    vec![],
                    vec![asset_path],
                    Some(2),
                )
                .unwrap(),
                20260715,
                5,
                None,
            )
            .unwrap_err();
        let unarchive_error = store
            .set_thema_archived("thema", 2, false, 6, None)
            .unwrap_err();

        assert_eq!(put_error.to_string(), "thema references an inactive asset");
        assert_eq!(put_error.to_string(), unarchive_error.to_string());
        let thema = store.get_thema("thema").unwrap().unwrap();
        assert_eq!(thema.revision, 2);
        assert!(thema.archived);
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
    fn reopened_document_retains_revisioned_node_source_spans() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("gnosis.db");
        let input = concat!(
            ":PROPERTIES:\n:ID: root\n:END:\n#+TITLE: Note\n",
            "* Child\n:PROPERTIES:\n:ID: child\n:END:\nbody\n",
        );
        let prepared = prepare_note(
            Some("document".into()),
            "note.org".into(),
            input.into(),
            None,
        )
        .unwrap();
        let projection = prepared.projection.clone();
        let blob = BlobMeta {
            sha256: prepared.blob_sha256.clone(),
            size_bytes: prepared.source.len() as u64,
            media_type: crate::parser::ORG_CONTENT_TYPE.into(),
        };
        let id = DocumentId::new("document").unwrap();
        let mut store = SqliteStore::open(&path).unwrap();
        store.replace_note(&prepared, &blob, 1, None).unwrap();
        drop(store);

        let mut reopened = SqliteStore::open(&path).unwrap();
        let stored = reopened.get_document(&id).unwrap().unwrap();

        assert_eq!(stored.projection(), &projection);
        assert!(
            stored
                .projection()
                .nodes()
                .iter()
                .all(|node| node.source_span().is_some())
        );
        let child = reopened.get_node("child").unwrap().unwrap();
        assert_eq!(child.document_revision, 1);
        assert_eq!(child.content_sha256, prepared.blob_sha256);
        assert_eq!((child.start_byte, child.end_byte), (Some(43), Some(86)));
        assert!(child.active);

        let replacement = prepare_note(
            Some("document".into()),
            "note.org".into(),
            ":PROPERTIES:\n:ID: root\n:END:\n#+TITLE: Note\n".into(),
            Some(1),
        )
        .unwrap();
        let replacement_blob = BlobMeta {
            sha256: replacement.blob_sha256.clone(),
            size_bytes: replacement.source.len() as u64,
            media_type: crate::parser::ORG_CONTENT_TYPE.into(),
        };
        reopened
            .replace_note(&replacement, &replacement_blob, 2, None)
            .unwrap();

        let retired = reopened.get_node("child").unwrap().unwrap();
        assert!(!retired.active);

        reopened.tombstone_document(&id, 2, 3, None).unwrap();
        let tombstoned = reopened.get_node("root").unwrap().unwrap();
        assert_eq!(tombstoned.document_revision, 2);
        assert_eq!(tombstoned.content_sha256, replacement.blob_sha256);
        assert_eq!((tombstoned.start_byte, tombstoned.end_byte), (None, None));
        assert!(!tombstoned.active);
    }

    #[test]
    fn node_search_reads_legacy_projection_compatibility_digest() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let prepared = prepare_note(
            Some("document".into()),
            "note.org".into(),
            "#+title: Note\n\n* Node\n:PROPERTIES:\n:ID: node\n:END:\n".into(),
            None,
        )
        .unwrap();
        let blob = BlobMeta {
            sha256: prepared.blob_sha256.clone(),
            size_bytes: prepared.source.len() as u64,
            media_type: crate::parser::ORG_CONTENT_TYPE.into(),
        };
        store.replace_note(&prepared, &blob, 1, None).unwrap();
        let stored: String = store
            .connection
            .query_row(
                "SELECT projection_json FROM document_projection
                 WHERE document_id = 'document'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let mut legacy: serde_json::Value = serde_json::from_str(&stored).unwrap();
        legacy["compatibility_sha1"] = serde_json::Value::String("a".repeat(40));
        for node in legacy["nodes"].as_array_mut().unwrap() {
            node.as_object_mut().unwrap().remove("start_byte");
            node.as_object_mut().unwrap().remove("end_byte");
        }
        store
            .connection
            .execute(
                "UPDATE document_projection SET projection_json = ?1
                 WHERE document_id = 'document'",
                [serde_json::to_string(&legacy).unwrap()],
            )
            .unwrap();

        let records = store.search_nodes("", 1, None).unwrap();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].node_id, "node");
        assert_eq!(records[0].document_revision, 1);
        assert_eq!(records[0].content_sha256, prepared.blob_sha256);
        assert_eq!((records[0].start_byte, records[0].end_byte), (None, None));
    }

    #[test]
    fn exact_tag_search_matches_current_and_retired_unicode_tags() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let first = prepare_note(
            Some("document".into()),
            "note.org".into(),
            "#+title: Note\n\n* Current :Δ:\n:PROPERTIES:\n:ID: current\n:END:\n\n* Retired :δ:\n:PROPERTIES:\n:ID: retired\n:END:\n".into(),
            None,
        )
        .unwrap();
        let first_blob = BlobMeta {
            sha256: first.blob_sha256.clone(),
            size_bytes: first.source.len() as u64,
            media_type: crate::parser::ORG_CONTENT_TYPE.into(),
        };
        store.replace_note(&first, &first_blob, 1, None).unwrap();
        let replacement = prepare_note(
            Some("document".into()),
            "note.org".into(),
            "#+title: Note\n\n* Current :Δ:\n:PROPERTIES:\n:ID: current\n:END:\n".into(),
            Some(1),
        )
        .unwrap();
        let replacement_blob = BlobMeta {
            sha256: replacement.blob_sha256.clone(),
            size_bytes: replacement.source.len() as u64,
            media_type: crate::parser::ORG_CONTENT_TYPE.into(),
        };
        store
            .replace_note(&replacement, &replacement_blob, 2, None)
            .unwrap();

        assert_eq!(
            store
                .search_nodes_by_tag("Δ", 10, None)
                .unwrap()
                .into_iter()
                .map(|node| node.node_id)
                .collect::<Vec<_>>(),
            ["current"]
        );
        assert_eq!(
            store
                .search_nodes_by_tag("δ", 10, None)
                .unwrap()
                .into_iter()
                .map(|node| node.node_id)
                .collect::<Vec<_>>(),
            ["retired"]
        );
    }

    #[test]
    fn content_search_reads_authoritative_bodies_in_document_order() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let mut sources = std::collections::HashMap::new();
        for (document_id, source) in [
            ("document-b", "#+title: B\nbody-only Needle.+\n"),
            ("document-a", "#+title: needle.+ title\nno match marker\n"),
            ("document-c", "#+title: B\nbody-only Needle.+\n"),
        ] {
            let prepared = prepare_note(
                Some(document_id.into()),
                format!("{document_id}.org"),
                source.into(),
                None,
            )
            .unwrap();
            let blob = BlobMeta {
                sha256: prepared.blob_sha256.clone(),
                size_bytes: prepared.source.len() as u64,
                media_type: crate::parser::ORG_CONTENT_TYPE.into(),
            };
            sources.insert(
                prepared.blob_sha256.clone(),
                prepared.source.clone().into_bytes(),
            );
            store.replace_note(&prepared, &blob, 1, None).unwrap();
        }

        let mut loads = 0;
        let page = store
            .search_document_content("NEEDLE.+", None, None, 100, |digest, _size| {
                loads += 1;
                sources.get(digest).cloned().ok_or(StoreError::CorruptData {
                    field: "document source",
                })
            })
            .unwrap();

        assert_eq!(
            page.items
                .iter()
                .map(|item| item.hit.document_id.as_str())
                .collect::<Vec<_>>(),
            ["document-a", "document-b", "document-c"]
        );
        assert_eq!(loads, 2, "equal source digests must be loaded once");
        assert!(
            page.items
                .iter()
                .all(|item| item.hit.file_node_id.is_none())
        );
        assert!(page.next_position.is_none());
    }

    #[test]
    fn node_catalog_is_compact_filterable_and_generation_bound() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let device = generate_device("Catalog".into(), vec!["read".into()]).unwrap();
        let device_id = device.device_id.clone();
        store.create_device(&device, 1).unwrap();
        let first = prepare_note(
            Some("document-a".into()),
            "notes/a.org".into(),
            concat!(
                ":PROPERTIES:\n:ID: a-root\n:END:\n#+title: A\n",
                "* Child\n:PROPERTIES:\n:ID: a-child\n:END:\n"
            )
            .into(),
            None,
        )
        .unwrap();
        let first_blob = BlobMeta {
            sha256: first.blob_sha256.clone(),
            size_bytes: first.source.len() as u64,
            media_type: crate::parser::ORG_CONTENT_TYPE.into(),
        };
        store.replace_note(&first, &first_blob, 2, None).unwrap();
        let second = prepare_note(
            Some("document-b".into()),
            "notes/b.org".into(),
            ":PROPERTIES:\n:ID: b-root\n:END:\n#+title: B\n".into(),
            None,
        )
        .unwrap();
        let second_blob = BlobMeta {
            sha256: second.blob_sha256.clone(),
            size_bytes: second.source.len() as u64,
            media_type: crate::parser::ORG_CONTENT_TYPE.into(),
        };
        store.replace_note(&second, &second_blob, 3, None).unwrap();
        let replacement = prepare_note(
            Some("document-a".into()),
            "notes/a.org".into(),
            ":PROPERTIES:\n:ID: a-root\n:END:\n#+title: A\n".into(),
            Some(1),
        )
        .unwrap();
        let replacement_blob = BlobMeta {
            sha256: replacement.blob_sha256.clone(),
            size_bytes: replacement.source.len() as u64,
            media_type: crate::parser::ORG_CONTENT_TYPE.into(),
        };
        store
            .replace_note(&replacement, &replacement_blob, 4, None)
            .unwrap();

        let first_page = store.node_catalog(&device_id, None, 2, None).unwrap();
        assert_eq!(first_page.state.cursor, 3);
        assert!(first_page.has_more);
        assert_eq!(
            first_page
                .items
                .iter()
                .map(|item| (item.node_id.as_str(), item.active))
                .collect::<Vec<_>>(),
            vec![("a-child", false), ("a-root", true)]
        );
        assert_eq!(first_page.items[0].logical_path, "notes/a.org");
        assert_eq!(first_page.items[0].title_path, ["A", "Child"]);

        let remainder = store
            .node_catalog(&device_id, None, 2, Some("a-root"))
            .unwrap();
        assert_eq!(remainder.state, first_page.state);
        assert_eq!(remainder.items[0].node_id, "b-root");
        assert!(!remainder.has_more);

        let filtered = store
            .node_catalog(&device_id, Some("document-a"), 10, None)
            .unwrap();
        assert_eq!(filtered.items.len(), 2);
        assert!(!filtered.has_more);
        assert!(
            filtered
                .items
                .iter()
                .all(|item| item.document_id == "document-a")
        );
    }

    #[test]
    fn thema_catalog_and_suspension_share_authoritative_review_state() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let device =
            generate_device("Manager".into(), vec!["read".into(), "review".into()]).unwrap();
        let device_id = device.device_id.clone();
        store.create_device(&device, 1).unwrap();
        store
            .put_thema(&thema("Question", None), 20260726, 2, None)
            .unwrap();
        let operation = prepare_device_desired_state_mutation(
            1,
            "suspend-thema".into(),
            device_id.clone(),
            DeviceDesiredStateMutation::SetThemaSuspended {
                thema_id: ThemaId::new("thema").unwrap(),
                expected_state_revision: 0,
                suspended: true,
            },
        )
        .unwrap();

        let applied = store
            .apply_device_desired_state_mutation(&operation, 3)
            .unwrap();
        assert!(!applied.replayed);
        assert!(matches!(
            applied.result,
            DeviceMutationResult::Applied {
                changed: true,
                authoritative: DeviceMutationProjection::ReviewState { ref state, .. },
                ..
            } if state.state_revision == 1 && state.suspended
        ));
        let replay = store
            .apply_device_desired_state_mutation(&operation, 4)
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.result, applied.result);

        let stale = prepare_device_desired_state_mutation(
            1,
            "stale-suspend".into(),
            device_id.clone(),
            DeviceDesiredStateMutation::SetThemaSuspended {
                thema_id: ThemaId::new("thema").unwrap(),
                expected_state_revision: 0,
                suspended: false,
            },
        )
        .unwrap();
        assert!(matches!(
            store
                .apply_device_desired_state_mutation(&stale, 5)
                .unwrap()
                .result,
            DeviceMutationResult::Rejected { .. }
        ));

        store
            .connection
            .execute(
                "UPDATE review_state
                 SET review_count = 7, total_successes = 7,
                     interval_days = 1, stability = 1.0, difficulty = 5.0",
                [],
            )
            .unwrap();
        store.set_thema_archived("thema", 1, true, 6, None).unwrap();
        let page = store.thema_catalog(&device_id, "", 10, None).unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].thema_revision, 2);
        assert_eq!(page.items[0].state_revision, 1);
        assert_eq!(page.items[0].review_count, 7);
        assert!(page.items[0].archived);
        assert!(page.items[0].suspended);
        let history: (i64, Option<String>, String) = store
            .connection
            .query_row(
                "SELECT state_revision, event_id, state_json
                 FROM review_state_revision
                 WHERE thema_id = 'thema'
                 ORDER BY state_revision DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(history.0, 1);
        assert_eq!(history.1, None);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&history.2).unwrap()["suspended"],
            true
        );
        assert!(
            store.content_changes(0, 100).unwrap().iter().any(|change| {
                change.entity_kind == "review" && change.operation == "suspension"
            })
        );
    }

    #[test]
    fn thema_catalog_stops_before_aggregate_materialization_exceeds_budget() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let device = generate_device("Reader".into(), vec!["read".into()]).unwrap();
        let device_id = device.device_id.clone();
        store.create_device(&device, 1).unwrap();
        for (index, id) in ["thema-a", "thema-b", "thema-c"].into_iter().enumerate() {
            store
                .put_thema(
                    &thema_named(id, &"x".repeat(128), None),
                    20260730,
                    i64::try_from(index + 2).unwrap(),
                    None,
                )
                .unwrap();
        }

        let first = store
            .thema_catalog_with_byte_budget(&device_id, "", 10, None, 300)
            .unwrap();
        assert_eq!(first.items.len(), 1);
        assert_eq!(first.items[0].thema_id, "thema-a");
        assert!(first.has_more);
        assert!(store.settings().is_ok());

        let second = store
            .thema_catalog_with_byte_budget(&device_id, "", 10, Some("thema-a"), 300)
            .unwrap();
        assert_eq!(second.items.len(), 1);
        assert_eq!(second.items[0].thema_id, "thema-b");
        assert!(second.has_more);
    }

    #[test]
    fn node_page_stops_before_aggregate_projection_decode_exceeds_its_budget() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let mut projection_size = 0;
        for index in 0..8 {
            let document_id = format!("document-{index}");
            let node_id = format!("node-{index}");
            let prepared = prepare_note(
                Some(document_id.clone()),
                format!("{document_id}.org"),
                format!("#+title: {document_id}\n\n* Node\n:PROPERTIES:\n:ID: {node_id}\n:END:\n"),
                None,
            )
            .unwrap();
            let blob = BlobMeta {
                sha256: prepared.blob_sha256.clone(),
                size_bytes: prepared.source.len() as u64,
                media_type: crate::parser::ORG_CONTENT_TYPE.into(),
            };
            store
                .replace_note(&prepared, &blob, i64::from(index + 1), None)
                .unwrap();
            let stored: String = store
                .connection
                .query_row(
                    "SELECT projection_json FROM document_projection
                     WHERE document_id = ?1",
                    [&document_id],
                    |row| row.get(0),
                )
                .unwrap();
            let mut padded: serde_json::Value = serde_json::from_str(&stored).unwrap();
            padded["title"] = serde_json::Value::String("x".repeat(180_000));
            let padded = serde_json::to_string(&padded).unwrap();
            projection_size = padded.len();
            let stored = if index == 5 {
                serde_json::to_string(&"x".repeat(projection_size)).unwrap()
            } else {
                padded
            };
            store
                .connection
                .execute(
                    "UPDATE document_projection SET projection_json = ?1
                     WHERE document_id = ?2",
                    rusqlite::params![stored, document_id],
                )
                .unwrap();
        }
        assert!(projection_size * 5 < MAX_DOCUMENT_PROJECTION_BYTES);
        assert!(projection_size * 6 > MAX_DOCUMENT_PROJECTION_BYTES);

        let page = store.search_nodes("", 500, None).unwrap();
        assert_eq!(page.len(), 5);
        assert_eq!(page.last().unwrap().node_id, "node-4");
    }

    #[test]
    fn initializes_and_reopens_database() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("gnosis.db");
        let store = SqliteStore::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        assert_eq!(application_id(&store.connection).unwrap(), APPLICATION_ID);
        drop(store);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let reopened = SqliteStore::open(&path).unwrap();
        assert_eq!(reopened.schema_version().unwrap(), SCHEMA_VERSION);
        assert_eq!(
            application_id(&reopened.connection).unwrap(),
            APPLICATION_ID
        );
    }

    #[test]
    fn canonical_schema_contains_document_content_types() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection.execute_batch(CURRENT_SCHEMA).unwrap();

        for table in ["document", "document_revision"] {
            let count: u64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_info(?1)
                     WHERE name = 'content_type' AND type = 'TEXT' AND \"notnull\" = 1
                       AND dflt_value IS NULL",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "{table} must require content_type");
        }
        assert!(!CURRENT_SCHEMA.contains("CHECK(content_type ="));
        assert_eq!(schema_version(&connection).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn rejects_prior_schema_versions_without_changes() {
        for version in [1, 2] {
            let connection = rusqlite::Connection::open_in_memory().unwrap();
            connection.execute_batch(CURRENT_SCHEMA).unwrap();
            connection
                .execute("INSERT INTO thema(thema_id) VALUES ('preserved')", [])
                .unwrap();
            connection
                .pragma_update(None, "user_version", version)
                .unwrap();
            let objects = schema_objects(&connection).unwrap();

            let error = initialize_schema(&connection).unwrap_err();

            assert!(matches!(
                error,
                StoreError::UnsupportedSchemaVersion {
                    found,
                    supported: SCHEMA_VERSION
                } if found == version
            ));
            assert_eq!(schema_version(&connection).unwrap(), version);
            assert_eq!(schema_objects(&connection).unwrap(), objects);
            assert_eq!(
                connection
                    .query_row("SELECT thema_id FROM thema", [], |row| row
                        .get::<_, String>(0))
                    .unwrap(),
                "preserved"
            );
        }
    }

    #[test]
    fn rejects_noncurrent_schema_without_changes() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE marker(value TEXT);
                 INSERT INTO marker VALUES ('preserved');
                 PRAGMA user_version = 5;",
            )
            .unwrap();

        let error = initialize_schema(&connection).unwrap_err();

        assert!(matches!(
            error,
            StoreError::UnsupportedSchemaVersion {
                found: 5,
                supported: SCHEMA_VERSION
            }
        ));
        assert_eq!(schema_version(&connection).unwrap(), 5);
        assert_eq!(
            connection
                .query_row("SELECT value FROM marker", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "preserved"
        );
    }

    #[test]
    fn rejects_stale_current_version_layout_without_changes() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection.execute_batch(CURRENT_SCHEMA).unwrap();
        connection
            .execute_batch(
                "ALTER TABLE document ADD COLUMN stale_value TEXT NOT NULL DEFAULT '';
                 CREATE TABLE stale_table(value TEXT) STRICT;
                 INSERT INTO stale_table VALUES ('preserved');",
            )
            .unwrap();

        let error = initialize_schema(&connection).unwrap_err();

        assert!(matches!(error, StoreError::UnsupportedSchemaLayout));
        assert_eq!(schema_version(&connection).unwrap(), SCHEMA_VERSION);
        assert_eq!(
            connection
                .query_row("SELECT value FROM stale_table", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
            "preserved"
        );
    }

    #[test]
    fn verification_observes_cancellation_before_sqlite_work() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("gnosis.sqlite");
        drop(SqliteStore::open(&database).unwrap());
        let cancelled = Arc::new(AtomicBool::new(true));

        let error =
            SqliteStore::verify_database_file_cancellable(&database, cancelled).unwrap_err();

        assert!(matches!(error, StoreError::BackupCancelled));
    }

    #[test]
    fn active_verification_observes_cancellation() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("gnosis.sqlite");
        drop(SqliteStore::open(&database).unwrap());
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let (started, started_receiver) = mpsc::sync_channel(0);
        let (release, release_receiver) = mpsc::sync_channel(0);
        let worker = thread::spawn(move || {
            SqliteStore::verify_database_file_cancellable_with(
                &database,
                worker_cancelled,
                move || {
                    started.send(()).unwrap();
                    release_receiver.recv().unwrap();
                },
            )
        });

        started_receiver.recv().unwrap();
        cancelled.store(true, Ordering::Release);
        release.send(()).unwrap();

        assert!(matches!(
            worker.join().unwrap(),
            Err(StoreError::BackupCancelled)
        ));
    }

    #[test]
    fn rejects_stale_current_version_backup_layout() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("backup.sqlite3");
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection.execute_batch(CURRENT_SCHEMA).unwrap();
        connection
            .execute_batch("CREATE TABLE stale_table(value TEXT) STRICT;")
            .unwrap();
        drop(connection);

        let error = SqliteStore::verify_database_file(&database).unwrap_err();

        assert!(matches!(error, StoreError::UnsupportedSchemaLayout));
    }

    #[test]
    fn rejects_unversioned_nonempty_database_without_changes() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection
            .execute("CREATE TABLE marker(value TEXT)", [])
            .unwrap();

        let error = initialize_schema(&connection).unwrap_err();

        assert!(matches!(
            error,
            StoreError::UnsupportedSchemaVersion {
                found: 0,
                supported: SCHEMA_VERSION
            }
        ));
        assert!(schema_table_exists(&connection, "marker"));
        assert!(!schema_table_exists(&connection, "document"));
    }

    #[test]
    fn rejects_wrong_application_id_without_changes() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        let wrong_application_id = APPLICATION_ID + 1;
        connection
            .execute_batch(&format!(
                "CREATE TABLE marker(value TEXT);
                 INSERT INTO marker VALUES ('preserved');
                 PRAGMA user_version = {SCHEMA_VERSION};
                 PRAGMA application_id = {wrong_application_id};"
            ))
            .unwrap();

        let error = initialize_schema(&connection).unwrap_err();

        assert!(matches!(
            error,
            StoreError::UnsupportedApplicationId {
                found,
                expected: APPLICATION_ID
            } if found == wrong_application_id
        ));
        assert_eq!(schema_version(&connection).unwrap(), SCHEMA_VERSION);
        assert_eq!(application_id(&connection).unwrap(), wrong_application_id);
        assert_eq!(
            connection
                .query_row("SELECT value FROM marker", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "preserved"
        );
    }

    fn schema_table_exists(connection: &rusqlite::Connection, table: &str) -> bool {
        connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1)",
                [table],
                |row| row.get(0),
            )
            .unwrap()
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
    fn current_heads_survive_empty_history_tables() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("gnosis.sqlite");
        let mut store = SqliteStore::open(&database).unwrap();
        let author = generate_device("Author".into(), vec!["read".into()]).unwrap();
        let other = generate_device("Other".into(), vec!["read".into()]).unwrap();
        store.create_device(&author, 1).unwrap();
        store.create_device(&other, 2).unwrap();
        let initial = store.replica_state(&author.device_id).unwrap();
        let source = ":PROPERTIES:\n:ID: root\n:END:\n#+title: Current\n* Child\n:PROPERTIES:\n:ID: child\n:END:\nΚαὶ [[id:root][Root]].\n";
        let note = prepare_note(
            Some("document".into()),
            "note.org".into(),
            source.into(),
            None,
        )
        .unwrap();
        let blob = BlobMeta {
            sha256: note.blob_sha256.clone(),
            size_bytes: note.source.len() as u64,
            media_type: crate::parser::ORG_CONTENT_TYPE.into(),
        };
        store
            .replace_note(&note, &blob, 3, Some(&author.device_id))
            .unwrap();
        store
            .replace_note(&note, &blob, 4, Some(&other.device_id))
            .unwrap();
        let prepared_thema = thema("Question", None);
        store
            .put_thema(&prepared_thema, 20260820, 5, Some(&author.device_id))
            .unwrap();
        store
            .put_thema(&prepared_thema, 20260820, 6, Some(&other.device_id))
            .unwrap();
        let projection_json: String = store
            .connection
            .query_row(
                "DELETE FROM document_projection WHERE document_id = 'document' RETURNING projection_json",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(store);
        let mut store = SqliteStore::open(&database).unwrap();
        let document_id = DocumentId::new("document").unwrap();
        assert!(
            [
                store.get_document(&document_id).map(|_| ()),
                store.get_node("root").map(|_| ()),
                store.search_nodes("Child", 20, None).map(|_| ()),
                store
                    .replica_changes(
                        &initial.server_id,
                        initial.cursor,
                        &initial.cursor_token,
                        20,
                    )
                    .map(|_| ()),
                store
                    .create_replica_snapshot(&author.device_id, 7, 20)
                    .map(|_| ()),
            ]
            .into_iter()
            .all(|result| matches!(
                result,
                Err(StoreError::CorruptData {
                    field: "document projection"
                })
            ))
        );
        store
            .connection
            .execute(
                "INSERT INTO document_projection(document_id, projection_json) VALUES ('document', ?1)",
                [&projection_json],
            )
            .unwrap();
        let mut oversized: serde_json::Value = serde_json::from_str(&projection_json).unwrap();
        oversized["title"] =
            serde_json::Value::String("x".repeat(MAX_DOCUMENT_PROJECTION_BYTES + 1));
        store
            .connection
            .execute(
                "UPDATE document_projection SET projection_json = ?1 WHERE document_id = 'document'",
                [serde_json::to_string(&oversized).unwrap()],
            )
            .unwrap();
        drop(store);
        let mut store = SqliteStore::open(&database).unwrap();
        assert!(
            [
                store.get_document(&document_id).map(|_| ()),
                store
                    .replica_changes(
                        &initial.server_id,
                        initial.cursor,
                        &initial.cursor_token,
                        20,
                    )
                    .map(|_| ()),
                store
                    .create_replica_snapshot(&author.device_id, 7, 20)
                    .map(|_| ()),
            ]
            .into_iter()
            .all(|result| matches!(
                result,
                Err(StoreError::CorruptData {
                    field: "document projection size"
                })
            ))
        );
        store
            .connection
            .execute(
                "UPDATE document_projection SET projection_json = ?1 WHERE document_id = 'document'",
                [projection_json],
            )
            .unwrap();
        store
            .connection
            .execute_batch(
                "DELETE FROM document_revision_asset;
                 DELETE FROM document_projection_revision;
                 DELETE FROM document_revision;
                 DELETE FROM thema_revision;",
            )
            .unwrap();
        drop(store);
        let mut store = SqliteStore::open(&database).unwrap();
        let (changes, _, _) = store
            .replica_changes(
                &initial.server_id,
                initial.cursor,
                &initial.cursor_token,
                20,
            )
            .unwrap();
        assert!(changes.iter().any(|change| {
            match &change.projection {
                ReplicaProjection::Document(value) => {
                    value
                        .revision
                        .author_device_id
                        .as_deref()
                        .is_some_and(|id| id == author.device_id.as_str())
                        && value.projection.as_ref().is_some_and(|projection| {
                            let link = &projection.link_occurrences[0];
                            &source[link.start..link.end] == "[[id:root][Root]]"
                        })
                }
                _ => false,
            }
        }));
        let thema = changes
            .iter()
            .find_map(|change| match &change.projection {
                ReplicaProjection::Thema(value) if value.record.thema_id == "thema" => Some(value),
                _ => None,
            })
            .unwrap();
        assert_eq!(thema.author_device_id, Some(author.device_id.clone()));
        let snapshot = store
            .create_replica_snapshot(&author.device_id, 7, 20)
            .unwrap();
        assert!(
            changes
                .iter()
                .all(|change| snapshot.items.contains(&change.projection))
        );

        let before_tombstone = store.replica_state(&author.device_id).unwrap();
        store
            .tombstone_document(&document_id, 1, 8, Some(&other.device_id))
            .unwrap();
        assert!(!store.get_node("root").unwrap().unwrap().active);
        assert!(store.search_nodes("Child", 20, None).is_ok());
        assert_eq!(
            store.connection.query_row(
                "SELECT COUNT(*) FROM document_projection WHERE document_id = 'document'",
                [],
                |row| row.get::<_, u64>(0),
            ),
            Ok(0)
        );
        let (changes, _, _) = store
            .replica_changes(
                &before_tombstone.server_id,
                before_tombstone.cursor,
                &before_tombstone.cursor_token,
                20,
            )
            .unwrap();
        assert!(changes.iter().any(|change| matches!(
            &change.projection,
            ReplicaProjection::Document(value)
                if value.revision.operation == "tombstone"
                    && value.revision.author_device_id.as_deref() == Some(other.device_id.as_str())
                    && value.revision.blob_sha256.is_none()
                    && value.projection.is_none()
        )));
        drop(store);
        assert!(SqliteStore::verify_database_file(&database).is_ok());
    }

    #[test]
    fn replica_projects_current_heads_and_body_free_tombstones() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("gnosis.sqlite");
        let mut store = SqliteStore::open(&database).unwrap();
        let device = generate_device("Replica".into(), vec!["read".into()]).unwrap();
        let device_id = device.device_id.clone();
        store.create_device(&device, 1).unwrap();
        let initial = store.replica_state(&device_id).unwrap();

        let first = prepare_note(
            Some("document".into()),
            "note.org".into(),
            ":PROPERTIES:\n:ID: root\n:END:\n#+title: First\n".into(),
            None,
        )
        .unwrap();
        let first_blob = BlobMeta {
            sha256: first.blob_sha256.clone(),
            size_bytes: first.source.len() as u64,
            media_type: "text/org; charset=utf-8".into(),
        };
        store.replace_note(&first, &first_blob, 2, None).unwrap();
        let second = prepare_note(
            Some("document".into()),
            "note.org".into(),
            ":PROPERTIES:\n:ID: root\n:END:\n#+title: Current\n".into(),
            Some(1),
        )
        .unwrap();
        let second_blob = BlobMeta {
            sha256: second.blob_sha256.clone(),
            size_bytes: second.source.len() as u64,
            media_type: "text/org; charset=utf-8".into(),
        };
        store
            .replace_note(&second, &second_blob, 3, Some(device_id.as_str()))
            .unwrap();

        let deleted = prepare_note(
            Some("deleted".into()),
            "deleted.org".into(),
            "#+title: Deleted\n".into(),
            None,
        )
        .unwrap();
        let deleted_blob = BlobMeta {
            sha256: deleted.blob_sha256.clone(),
            size_bytes: deleted.source.len() as u64,
            media_type: "text/org; charset=utf-8".into(),
        };
        store
            .replace_note(&deleted, &deleted_blob, 4, None)
            .unwrap();
        store
            .tombstone_document(&DocumentId::new("deleted").unwrap(), 1, 5, None)
            .unwrap();

        store
            .put_thema(&thema("First", None), 20260715, 6, None)
            .unwrap();
        store
            .put_thema(
                &thema("Current", Some(1)),
                20260715,
                7,
                Some(device_id.as_str()),
            )
            .unwrap();
        store
            .put_thema(
                &thema_named("fallback", "Fallback", None),
                20260715,
                8,
                Some(device_id.as_str()),
            )
            .unwrap();
        store
            .connection
            .execute("DELETE FROM thema_revision WHERE thema_id = 'fallback'", [])
            .unwrap();
        drop(store);
        let mut store = SqliteStore::open(&database).unwrap();

        let (changes, _, _) = store
            .replica_changes(
                &initial.server_id,
                initial.cursor,
                &initial.cursor_token,
                20,
            )
            .unwrap();
        let documents = changes
            .iter()
            .filter_map(|change| match &change.projection {
                ReplicaProjection::Document(value) => Some(value),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(documents.len(), 2);
        let current = documents
            .iter()
            .find(|document| document.revision.document_id == "document")
            .unwrap();
        assert_eq!(current.revision.revision, 2);
        assert_eq!(
            current.revision.author_device_id.as_deref(),
            Some(device_id.as_str())
        );
        assert_eq!(current.projection.as_ref().unwrap().title, "Current");
        let root = &current.projection.as_ref().unwrap().nodes[0];
        assert!(root.start_byte.is_some() && root.end_byte.is_some());
        let tombstone = documents
            .iter()
            .find(|document| document.revision.document_id == "deleted")
            .unwrap();
        assert_eq!(tombstone.revision.operation, "tombstone");
        assert!(tombstone.revision.blob_sha256.is_none());
        assert!(tombstone.projection.is_none());
        let themata = changes
            .iter()
            .filter_map(|change| match &change.projection {
                ReplicaProjection::Thema(value) => Some(value),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(themata.len(), 2);
        let current = themata
            .iter()
            .find(|value| value.record.thema_id == "thema")
            .unwrap();
        assert_eq!(current.record.revision, 2);
        assert_eq!(current.record.content.keimenon, "Current");
        assert_eq!(
            current.author_device_id.as_deref(),
            Some(device_id.as_str())
        );
        assert_eq!(
            themata
                .iter()
                .find(|value| value.record.thema_id == "fallback")
                .unwrap()
                .author_device_id
                .as_deref(),
            Some(device_id.as_str())
        );

        let mut page = store.create_replica_snapshot(&device_id, 9, 1).unwrap();
        let mut snapshot = Vec::new();
        loop {
            assert!(page.items.len() <= 1);
            snapshot.extend(page.items);
            let Some(position) = page.next else {
                break;
            };
            page = store
                .replica_snapshot_page(&device_id, &position, 1)
                .unwrap();
        }
        assert!(snapshot.iter().any(|item| matches!(
            item,
            ReplicaProjection::Document(value)
                if value.revision.document_id == "document"
                    && value.revision.author_device_id.as_deref() == Some(device_id.as_str())
                    && value.projection.as_ref().is_some_and(|value| value.title == "Current")
        )));
        assert!(snapshot.iter().any(|item| matches!(
            item,
            ReplicaProjection::Thema(value)
                if value.record.content.keimenon == "Current"
                    && value.author_device_id.as_deref() == Some(device_id.as_str())
        )));
        assert_eq!(
            store
                .connection
                .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
        assert!(
            !store
                .connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM pragma_foreign_key_check)",
                    [],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap()
        );
    }

    #[test]
    fn snapshot_pages_project_a_head_updated_after_the_cut() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let device = generate_device("Replica".into(), vec!["read".into()]).unwrap();
        let device_id = device.device_id.clone();
        store.create_device(&device, 1).unwrap();

        for (id, path) in [
            ("a-leading", "a.org"),
            ("b-current", "b.org"),
            ("c-tombstone", "c.org"),
        ] {
            let note = prepare_note(
                Some(id.into()),
                path.into(),
                format!("#+title: {id}\n"),
                None,
            )
            .unwrap();
            let blob = BlobMeta {
                sha256: note.blob_sha256.clone(),
                size_bytes: note.source.len() as u64,
                media_type: crate::parser::ORG_CONTENT_TYPE.into(),
            };
            store.replace_note(&note, &blob, 2, None).unwrap();
        }
        store
            .put_thema(&thema("Before", None), 20260715, 3, None)
            .unwrap();

        let first = store.create_replica_snapshot(&device_id, 4, 1).unwrap();
        assert!(matches!(
            &first.items[0],
            ReplicaProjection::Document(value)
                if value.revision.document_id == "a-leading"
        ));

        let current_source = concat!(
            ":PROPERTIES:\n:ID: current-root\n:END:\n",
            "#+title: Current\nSee [[id:target][Target]].\n"
        );
        let current = prepare_note(
            Some("b-current".into()),
            "b.org".into(),
            current_source.into(),
            Some(1),
        )
        .unwrap();
        let current_blob = BlobMeta {
            sha256: current.blob_sha256.clone(),
            size_bytes: current.source.len() as u64,
            media_type: crate::parser::ORG_CONTENT_TYPE.into(),
        };
        store
            .replace_note(&current, &current_blob, 5, Some(device_id.as_str()))
            .unwrap();
        store
            .tombstone_document(
                &DocumentId::new("c-tombstone").unwrap(),
                1,
                6,
                Some(device_id.as_str()),
            )
            .unwrap();
        store
            .put_thema(
                &thema("Current", Some(1)),
                20260715,
                7,
                Some(device_id.as_str()),
            )
            .unwrap();

        let mut items = first.items;
        let mut position = first.next.unwrap();
        loop {
            let page = store
                .replica_snapshot_page(&device_id, &position, 1)
                .unwrap();
            assert_eq!(page.items.len(), 1);
            items.extend(page.items);
            let Some(next) = page.next else {
                break;
            };
            position = next;
        }

        let current = items
            .iter()
            .find_map(|item| match item {
                ReplicaProjection::Document(value) if value.revision.document_id == "b-current" => {
                    Some(value)
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(current.revision.revision, 2);
        assert_eq!(
            current.revision.author_device_id.as_deref(),
            Some(device_id.as_str())
        );
        let projection = current.projection.as_ref().unwrap();
        assert_eq!(projection.title, "Current");
        let link = &projection.link_occurrences[0];
        let link_start = current_source.find("[[id:target][Target]]").unwrap();
        assert_eq!((link.start, link.end), (link_start, link_start + 21));

        let tombstone = items
            .iter()
            .find_map(|item| match item {
                ReplicaProjection::Document(value)
                    if value.revision.document_id == "c-tombstone" =>
                {
                    Some(value)
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(tombstone.revision.operation, "tombstone");
        assert!(tombstone.revision.blob_sha256.is_none());
        assert!(tombstone.projection.is_none());
        assert_eq!(
            tombstone.revision.author_device_id.as_deref(),
            Some(device_id.as_str())
        );

        let thema = items
            .iter()
            .find_map(|item| match item {
                ReplicaProjection::Thema(value) => Some(value),
                _ => None,
            })
            .unwrap();
        assert_eq!(thema.record.revision, 2);
        assert_eq!(thema.record.content.keimenon, "Current");
        assert_eq!(thema.author_device_id.as_deref(), Some(device_id.as_str()));
        assert_eq!(
            store
                .connection
                .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
        assert!(
            !store
                .connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM pragma_foreign_key_check)",
                    [],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap()
        );
    }

    #[test]
    fn snapshot_rejects_a_head_older_than_its_manifest_revision() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let device = generate_device("Replica".into(), vec!["read".into()]).unwrap();
        let device_id = device.device_id.clone();
        store.create_device(&device, 1).unwrap();
        let first = prepare_note(
            Some("document".into()),
            "note.org".into(),
            "#+title: First\n".into(),
            None,
        )
        .unwrap();
        let first_blob = BlobMeta {
            sha256: first.blob_sha256.clone(),
            size_bytes: first.source.len() as u64,
            media_type: crate::parser::ORG_CONTENT_TYPE.into(),
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
            size_bytes: second.source.len() as u64,
            media_type: crate::parser::ORG_CONTENT_TYPE.into(),
        };
        store.replace_note(&second, &second_blob, 3, None).unwrap();
        let snapshot = store.create_replica_snapshot(&device_id, 4, 1).unwrap();
        store
            .connection
            .execute(
                "UPDATE document SET revision = 1 WHERE document_id = 'document'",
                [],
            )
            .unwrap();

        assert!(matches!(
            store.replica_snapshot_page(&device_id, &snapshot.position, 1),
            Err(StoreError::InvalidServerState)
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
        assert_eq!(snapshot_settings.settings.timezone, "Europe/Athens");

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
            size_bytes: first.source.len() as u64,
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
            size_bytes: second.source.len() as u64,
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
        assert_eq!(documents.len(), 1);
        assert!(
            documents
                .iter()
                .all(|document| { document.revision.content_type == "text/org; charset=utf-8" })
        );
        assert_eq!(documents[0].revision.operation, "tombstone");
        assert!(documents[0].projection.is_none());

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
        assert_eq!(themata.len(), 1);
        assert_eq!(themata[0].record.content.keimenon, "Second");

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
    fn replica_changes_bounds_stale_head_scanning() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let device = generate_device("Replica".into(), vec!["read".into()]).unwrap();
        let device_id = device.device_id.clone();
        store.create_device(&device, 1).unwrap();
        let initial = store.replica_state(&device_id).unwrap();

        let transaction = store.connection.transaction().unwrap();
        {
            let mut insert = transaction
                .prepare(
                    "INSERT INTO content_change(
                         entity_kind, entity_id, revision, operation, created_at_us
                     ) VALUES ('document', 'missing', ?1, 'update', 2)",
                )
                .unwrap();
            for revision in 1..=4_097 {
                insert.execute([revision]).unwrap();
            }
        }
        transaction.commit().unwrap();
        let note = prepare_note(
            Some("current".into()),
            "current.org".into(),
            "#+title: Current\n".into(),
            None,
        )
        .unwrap();
        let blob = BlobMeta {
            sha256: note.blob_sha256.clone(),
            size_bytes: note.source.len() as u64,
            media_type: crate::parser::ORG_CONTENT_TYPE.into(),
        };
        store.replace_note(&note, &blob, 3, None).unwrap();

        assert!(matches!(
            store.replica_changes(&initial.server_id, initial.cursor, &initial.cursor_token, 1,),
            Err(StoreError::ReplicaCursor)
        ));
    }

    #[test]
    fn replica_changes_never_returns_a_nonadvancing_stale_page() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let device = generate_device("Replica".into(), vec!["read".into()]).unwrap();
        let device_id = device.device_id.clone();
        store.create_device(&device, 1).unwrap();
        let initial = store.replica_state(&device_id).unwrap();
        let note = prepare_note(
            Some("current".into()),
            "current.org".into(),
            "#+title: Current\n".into(),
            None,
        )
        .unwrap();
        let blob = BlobMeta {
            sha256: note.blob_sha256.clone(),
            size_bytes: note.source.len() as u64,
            media_type: crate::parser::ORG_CONTENT_TYPE.into(),
        };
        store.replace_note(&note, &blob, 2, None).unwrap();
        for revision in 1..=3 {
            store
                .connection
                .execute(
                    "INSERT INTO content_change(
                         entity_kind, entity_id, revision, operation, created_at_us
                     ) VALUES ('document', 'missing', ?1, 'update', 3)",
                    [revision],
                )
                .unwrap();
        }

        let (first, _, _) = store
            .replica_changes(&initial.server_id, initial.cursor, &initial.cursor_token, 1)
            .unwrap();
        assert_eq!(first.len(), 1);
        assert!(matches!(
            store.replica_changes(
                &initial.server_id,
                first[0].cursor,
                &first[0].cursor_token,
                1,
            ),
            Err(StoreError::ReplicaCursor)
        ));

        let latest = store.replica_state(&device_id).unwrap();
        let (empty, _, _) = store
            .replica_changes(&latest.server_id, latest.cursor, &latest.cursor_token, 1)
            .unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn replica_changes_resume_after_live_head_overwrite() {
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
            size_bytes: first.source.len() as u64,
            media_type: "text/org; charset=utf-8".into(),
        };
        store.replace_note(&first, &first_blob, 2, None).unwrap();
        let after_first = store.replica_state(&device_id).unwrap();

        let second = prepare_note(
            Some("document".into()),
            "note.org".into(),
            "#+title: Second\n".into(),
            Some(1),
        )
        .unwrap();
        let second_blob = BlobMeta {
            sha256: second.blob_sha256.clone(),
            size_bytes: second.source.len() as u64,
            media_type: "text/org; charset=utf-8".into(),
        };
        store.replace_note(&second, &second_blob, 3, None).unwrap();

        let from_mid = store
            .replica_changes(
                &after_first.server_id,
                after_first.cursor,
                &after_first.cursor_token,
                10,
            )
            .unwrap();
        assert_eq!(from_mid.1, after_first.cursor + 1);
        assert_eq!(from_mid.0.len(), 1);

        let (changes, _, _) = store
            .replica_changes(&initial.server_id, initial.cursor, &initial.cursor_token, 1)
            .unwrap();
        assert_eq!(changes.len(), 1);
        let ReplicaProjection::Document(document) = &changes[0].projection else {
            panic!("expected document change");
        };
        assert_eq!(document.revision.revision, 2);
        assert_eq!(
            document.revision.blob_sha256.as_deref(),
            Some(second.blob_sha256.as_str())
        );
        assert_ne!(
            document.revision.blob_sha256.as_deref(),
            Some(first.blob_sha256.as_str())
        );
    }

    #[test]
    fn thema_concepts_round_trip_through_search_history_snapshot_and_replacement() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let device = generate_device("Replica".into(), vec!["read".into()]).unwrap();
        let device_id = device.device_id.clone();
        store.create_device(&device, 1).unwrap();
        let initial = store.replica_state(&device_id).unwrap();
        let document_id = DocumentId::new("concept-document").unwrap();
        store
            .replace_document(&document_id, &concept_projection(), None)
            .unwrap();

        let first = prepare_thema(
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
            vec![
                ("primary".into(), ThemaConceptRelation::Primary),
                ("supporting".into(), ThemaConceptRelation::Supporting),
                ("reference".into(), ThemaConceptRelation::Reference),
            ],
            vec![],
            None,
        )
        .unwrap();
        store.put_thema(&first, 20260715, 2, None).unwrap();
        assert_eq!(
            store
                .get_thema("thema")
                .unwrap()
                .unwrap()
                .concepts
                .iter()
                .map(|concept| (concept.concept_id().to_string(), concept.relation()))
                .collect::<Vec<_>>(),
            [
                ("primary".into(), ThemaConceptRelation::Primary),
                ("reference".into(), ThemaConceptRelation::Reference),
                ("supporting".into(), ThemaConceptRelation::Supporting),
            ]
        );
        assert_eq!(
            store.search_themata("question", 10, None).unwrap()[0]
                .concepts
                .len(),
            3
        );
        for node_id in ["primary", "supporting", "reference"] {
            let source = ReviewSource {
                kind: ReviewSourceKind::Topic,
                tags: vec![],
                exclude_tags: vec![],
                topic_node_id: Some(node_id.into()),
                topic_forward_depth: Some(0),
                topic_backward_depth: Some(0),
                manual_ids: vec![],
            };
            assert_eq!(
                super::review::select_queue(&store.connection, &source, 20260715, None).unwrap(),
                ["thema"],
                "{node_id}"
            );
        }

        let replacement = projection("Concepts", "replacement");
        store
            .connection
            .execute(
                "UPDATE document SET logical_path = 'concepts.org', path_key = 'concepts.org' \
                 WHERE document_id = ?1",
                [document_id.as_str()],
            )
            .unwrap();
        let prepared_replacement = PreparedNote {
            document_id: document_id.clone(),
            explicit_document_id: true,
            logical_path_is_request_input: true,
            logical_path: "concepts.org".into(),
            path_key: "concepts.org".into(),
            content_type: "text/org; charset=utf-8".into(),
            source: String::new(),
            blob_sha256: "replacement".into(),
            asset_paths: vec![],
            projection: replacement,
            expected_revision: Some(1),
        };
        let transaction = store.connection.transaction().unwrap();
        retire_removed_nodes(&transaction, &prepared_replacement, 2).unwrap();
        replace_rows(
            &transaction,
            &document_id,
            &prepared_replacement.projection,
            crate::parser::ORG_CONTENT_TYPE,
            2,
            FailPoint::None,
        )
        .unwrap();
        transaction.commit().unwrap();
        let retired = store.get_node("supporting").unwrap().unwrap();
        assert!(!retired.active);
        assert_eq!(
            store.node_themata("supporting", 10, None).unwrap(),
            [NodeThemaAssociation {
                thema_id: "thema".into(),
                relation: ThemaConceptRelation::Supporting,
            }]
        );

        let second = prepare_thema(
            Some("thema".into()),
            thema_content(
                "basic",
                "Updated".into(),
                vec![],
                vec!["Answer".into()],
                None,
                None,
                None,
            )
            .unwrap(),
            vec![],
            vec![("root".into(), ThemaConceptRelation::Reference)],
            vec![],
            Some(1),
        )
        .unwrap();
        store.put_thema(&second, 20260715, 3, None).unwrap();
        assert!(
            store
                .node_themata("supporting", 10, None)
                .unwrap()
                .is_empty()
        );

        let state = store.replica_state(&device_id).unwrap();
        let (changes, _, _) = store
            .replica_changes(&state.server_id, 0, &initial.cursor_token, 20)
            .unwrap();
        let revisions = changes
            .iter()
            .filter_map(|change| match &change.projection {
                ReplicaProjection::Thema(value) => Some(&value.record),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(revisions.len(), 1);
        assert_eq!(
            revisions[0].concepts[0].relation(),
            ThemaConceptRelation::Reference
        );

        let snapshot = store.create_replica_snapshot(&device_id, 4, 100).unwrap();
        let snapshot_thema = snapshot.items.iter().find_map(|item| match item {
            ReplicaProjection::Thema(value) => Some(&value.record),
            _ => None,
        });
        assert_eq!(snapshot_thema.unwrap().concepts, revisions[0].concepts);
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
    fn stale_put_overwrites_current_projection() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let id = DocumentId::new("document").unwrap();
        store
            .replace_document(&id, &projection("First", "digest-one"), None)
            .unwrap();
        store
            .replace_document(&id, &projection("Second", "digest-two"), Some(1))
            .unwrap();
        let third = projection("Third", "digest-three");
        store.replace_document(&id, &third, Some(1)).unwrap();
        assert_eq!(
            store.get_document(&id).unwrap().unwrap().projection(),
            &third
        );
    }

    #[test]
    fn document_puts_keep_one_revision_row() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let id = DocumentId::new("document").unwrap();
        for (source, at) in [("First", 1), ("Second", 2), ("Third", 3)] {
            let note = crate::application::prepare_note(
                Some("document".into()),
                "note.org".into(),
                format!("#+title: {source}\n"),
                None,
            )
            .unwrap();
            let blob = BlobMeta {
                sha256: note.blob_sha256.clone(),
                size_bytes: note.source.len() as u64,
                media_type: crate::parser::ORG_CONTENT_TYPE.into(),
            };
            store.replace_note(&note, &blob, at, None).unwrap();
        }
        assert_eq!(store.document_revisions(&id, 0, 10).unwrap().len(), 1);
    }

    fn note_with_asset_link(
        document: &str,
        digest: &str,
        revision: Option<u64>,
    ) -> crate::application::PreparedNote {
        use crate::application::prepare_note;
        let org = format!("#+title: Note\n[[file:assets/{digest}/asset.png]]\n");
        prepare_note(Some(document.into()), "note.org".into(), org, revision).unwrap()
    }

    fn register_asset(store: &mut SqliteStore, bytes: &[u8]) -> String {
        use crate::application::prepare_asset;
        let digest = format!("{:x}", sha2::Sha256::digest(bytes));
        let path = format!("assets/{digest}/asset.png");
        let prepared = prepare_asset(path, "asset.png".into(), None).unwrap();
        let blob = BlobMeta {
            sha256: digest.clone(),
            size_bytes: bytes.len() as u64,
            media_type: "image/png".into(),
        };
        store.put_asset(&prepared, &blob, 1, None).unwrap();
        digest
    }

    #[test]
    fn repair_pages_use_indexed_keyset_queries() {
        let store = SqliteStore::open_in_memory().unwrap();
        for query in [
            "SELECT content_sha256, size_bytes FROM blob
             WHERE content_sha256 > '' ORDER BY content_sha256 LIMIT 1",
            "SELECT content_sha256, size_bytes FROM staged_blob
             WHERE content_sha256 > '' ORDER BY content_sha256 LIMIT 1",
            "SELECT operation_id, content_sha256, size_bytes FROM offline_asset_evidence
             WHERE operation_id > '' ORDER BY operation_id LIMIT 1",
            "SELECT document.document_id, document.head_blob_sha256,
                    document.content_type, document.active = 1
             FROM document
             WHERE document.document_id > '' ORDER BY document.document_id LIMIT 1",
        ] {
            let mut statement = store
                .connection
                .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
                .unwrap();
            let details: Vec<String> = statement
                .query_map([], |row| row.get(3))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            assert!(details.iter().any(|detail| detail.contains("SEARCH")));
            assert!(
                details
                    .iter()
                    .all(|detail| !detail.contains("USE TEMP B-TREE")),
                "unbounded repair query plan: {details:?}"
            );
        }
    }

    #[test]
    fn paged_repair_reconciles_current_associations() {
        use crate::application::retained_asset_paths;
        let mut store = SqliteStore::open_in_memory().unwrap();

        let digest_one = register_asset(&mut store, b"asset-one");
        let first = note_with_asset_link("document", &digest_one, None);
        let first_blob = BlobMeta {
            sha256: first.blob_sha256.clone(),
            size_bytes: first.source.len() as u64,
            media_type: "text/org; charset=utf-8".into(),
        };
        store.replace_note(&first, &first_blob, 2, None).unwrap();

        let digest_two = register_asset(&mut store, b"asset-two");
        let second = note_with_asset_link("document", &digest_two, Some(1));
        let second_blob = BlobMeta {
            sha256: second.blob_sha256.clone(),
            size_bytes: second.source.len() as u64,
            media_type: "text/org; charset=utf-8".into(),
        };
        store.replace_note(&second, &second_blob, 3, None).unwrap();

        let _first_assets = retained_asset_paths(&first.content_type, &first.source).unwrap();
        let second_assets = retained_asset_paths(&second.content_type, &second.source).unwrap();
        let document_id = DocumentId::new("document").unwrap();
        store.tombstone_document(&document_id, 2, 4, None).unwrap();
        store
            .connection
            .execute(
                "DELETE FROM document_revision_asset WHERE document_id = ?1",
                [document_id.as_str()],
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO document_asset(document_id, asset_path) VALUES (?1, ?2)",
                rusqlite::params![document_id.as_str(), second_assets[0]],
            )
            .unwrap();

        let heads = store.document_asset_heads_page(None, 1).unwrap();
        assert_eq!(heads.len(), 1);
        store
            .repair_document_assets(&[DocumentAssetAssociation {
                document_id: heads[0].document_id.clone(),
                asset_paths: Vec::new(),
                active_head: heads[0].active_head,
            }])
            .unwrap();
        let stale_current: u32 = store
            .connection
            .query_row(
                "SELECT count(*) FROM document_asset WHERE document_id = ?1",
                [document_id.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stale_current, 0);

        let mut statement = store
            .connection
            .prepare(
                "SELECT asset_path FROM document_asset WHERE document_id = ?1 ORDER BY asset_path",
            )
            .unwrap();
        let current_assets: Vec<String> = statement
            .query_map([document_id.as_str()], |row| row.get::<_, String>(0))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert!(current_assets.is_empty());
    }
}
