use std::{
    collections::HashSet, error::Error, fs, os::unix::fs::PermissionsExt, path::Path,
    time::Duration,
};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use thiserror::Error;

use crate::domain::{
    DocumentId, DocumentProjection, DocumentProjectionInput, DueThema, LegacyDatabaseInfo,
    LegacyReviewState, LegacyThema, LinkOccurrence, Node, NodeEdge, NodeId, SourceSpan, ThemaId,
    TitlePath,
};
use crate::{
    application::{PreparedAsset, PreparedNote},
    blob::BlobMeta,
};

const INITIAL_MIGRATION: &str = include_str!("migrations/0001_initial.sql");
const SERVER_MIGRATION: &str = include_str!("migrations/0002_server.sql");
const SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Error)]
pub(crate) enum StoreError {
    #[error("document revision conflict: expected {expected}, current revision is {actual:?}")]
    Conflict { expected: u64, actual: Option<u64> },
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
        let current = load_document(&transaction, &prepared.document_id)?;
        check_revision(prepared.expected_revision, current.as_ref())?;
        check_path_available(&transaction, prepared)?;
        if current.as_ref().is_some_and(|stored| {
            stored.projection() == &prepared.projection
                && document_path(&transaction, &prepared.document_id)
                    .ok()
                    .as_deref()
                    == Some(prepared.logical_path.as_str())
        }) {
            transaction.commit().map_err(StoreError::storage)?;
            return current.ok_or(StoreError::CorruptData { field: "document" });
        }
        let revision = next_revision(current.as_ref())?;
        retire_removed_nodes(&transaction, prepared, revision)?;
        insert_blob(&transaction, blob, created_at_us)?;
        replace_rows(
            &transaction,
            &prepared.document_id,
            &prepared.projection,
            revision,
            FailPoint::None,
        )?;
        update_note_head(&transaction, prepared, blob, revision, created_at_us)?;
        insert_document_revision(
            &transaction,
            prepared,
            revision,
            current.is_some(),
            created_at_us,
            author_device_id,
        )?;
        insert_content_change(
            &transaction,
            "document",
            prepared.document_id.as_str(),
            revision,
            if current.is_some() {
                "update"
            } else {
                "create"
            },
            created_at_us,
        )?;
        transaction.commit().map_err(StoreError::storage)?;
        Ok(StoredDocument {
            document_id: prepared.document_id.clone(),
            revision,
            projection: prepared.projection.clone(),
        })
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
        let current: Option<(String, i64)> = transaction
            .query_row(
                "SELECT blob_sha256, revision FROM managed_asset WHERE asset_path = ?1",
                [&prepared.logical_path],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(StoreError::storage)?;
        check_expected_number(
            prepared.expected_revision,
            current.as_ref().map(|(_, revision)| *revision),
        )?;
        if current
            .as_ref()
            .is_some_and(|(digest, _)| digest == &blob.sha256)
        {
            transaction.commit().map_err(StoreError::storage)?;
            return current
                .and_then(|(_, revision)| u64::try_from(revision).ok())
                .ok_or(StoreError::CorruptData {
                    field: "asset revision",
                });
        }
        let revision = next_number(current.as_ref().map(|(_, revision)| *revision))?;
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

    pub(crate) fn rewrite_thema_asset(
        &mut self,
        thema_id: &str,
        old_reference: &str,
        asset_path: &str,
    ) -> Result<(), StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(StoreError::storage)?;
        transaction
            .execute(
                "UPDATE thema SET review_image = ?3
                 WHERE thema_id = ?1 AND review_image = ?2",
                params![thema_id, old_reference, asset_path],
            )
            .map_err(StoreError::storage)?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO thema_asset(thema_id, asset_path, role)
                 VALUES (?1, ?2, 'review_image')",
                params![thema_id, asset_path],
            )
            .map_err(StoreError::storage)?;
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

fn document_path(
    transaction: &Transaction<'_>,
    document_id: &DocumentId,
) -> Result<String, StoreError> {
    transaction
        .query_row(
            "SELECT logical_path FROM document WHERE document_id = ?1",
            [document_id.as_str()],
            |row| row.get(0),
        )
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
    let logical_path = document_path(transaction, &prepared.document_id).ok();
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

    use tempfile::tempdir;

    use super::{FailPoint, SqliteStore, StoreError};
    use crate::domain::{
        DocumentId, DocumentProjection, DocumentProjectionInput, LinkOccurrence, Node, NodeId,
        SourceSpan, TitlePath,
    };

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
            2
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            SqliteStore::open(&path).unwrap().schema_version().unwrap(),
            2
        );
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
