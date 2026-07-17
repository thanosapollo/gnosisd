//! Atomic offline import of a legacy Emacs Gnosis database.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rusqlite::{
    Connection, OpenFlags, Transaction, TransactionBehavior, params, params_from_iter,
    types::{Value as SqlValue, ValueRef},
};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    application::{
        canonical_asset_path, path_key, prepare_asset, prepare_note, supported_asset,
        validate_logical_path,
    },
    blob::BlobStore,
    store::{SqliteStore, initialize_native_database, legacy_value},
};

const LEGACY_SCHEMA_VERSION: u32 = 9;

/// Summary of a completed legacy database import.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LegacyMigrationSummary {
    source_schema_version: u32,
    source_sha256: String,
    legacy_table_count: u64,
    legacy_row_count: u64,
    thema_count: u64,
    note_count: u64,
    asset_count: u64,
}

impl LegacyMigrationSummary {
    /// Return the imported Emacs database schema version.
    pub const fn source_schema_version(&self) -> u32 {
        self.source_schema_version
    }

    /// Return the SHA-256 digest of the source snapshot.
    pub fn source_sha256(&self) -> &str {
        &self.source_sha256
    }

    /// Return the number of validated legacy tables.
    pub const fn legacy_table_count(&self) -> u64 {
        self.legacy_table_count
    }

    /// Return the total number of validated legacy rows.
    pub const fn legacy_row_count(&self) -> u64 {
        self.legacy_row_count
    }

    /// Return the number of imported themata.
    pub const fn thema_count(&self) -> u64 {
        self.thema_count
    }

    /// Return the number of imported Org documents.
    pub const fn note_count(&self) -> u64 {
        self.note_count
    }

    /// Return the number of imported logical assets.
    pub const fn asset_count(&self) -> u64 {
        self.asset_count
    }
}

/// Failure while importing a legacy Emacs Gnosis database.
#[derive(Debug, Error)]
pub enum LegacyMigrationError {
    #[error("source database is not a regular file")]
    InvalidSource,
    #[error("source and target database paths must differ")]
    SameDatabase,
    #[error("source database is currently leased by Emacs")]
    SourceLeased,
    #[error("legacy database has an active review session")]
    ActiveReviewSession,
    #[error("target database already exists")]
    TargetExists,
    #[error("migration staging database already exists")]
    StagingExists,
    #[error("interrupted publication belongs to different targets")]
    PublicationTargetMismatch,
    #[error("legacy database schema version {found} is unsupported")]
    UnsupportedSourceSchema { found: u32 },
    #[error("legacy database schema is incomplete")]
    IncompleteSourceSchema,
    #[error("source database failed integrity validation")]
    SourceIntegrity,
    #[error("imported database failed integrity validation")]
    TargetIntegrity,
    #[error("legacy row counts changed during import")]
    ManifestMismatch,
    #[error("legacy database contains an invalid {field}")]
    InvalidLegacyValue { field: &'static str },
    #[error("Notes tree contains an invalid path, symlink, or local reference")]
    InvalidNotesTree,
    #[error("Notes tree contains invalid Org at {path}")]
    InvalidOrg { path: String },
    #[error("blob storage operation failed")]
    Blob,
    #[error("normalized storage operation failed")]
    Storage,
    #[error("migration filesystem operation failed")]
    Io(#[source] io::Error),
    #[error("migration database operation failed")]
    Database(#[source] rusqlite::Error),
    #[error("migration serialization failed")]
    Serialization(#[source] serde_json::Error),
}

impl From<io::Error> for LegacyMigrationError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for LegacyMigrationError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

impl From<serde_json::Error> for LegacyMigrationError {
    fn from(error: serde_json::Error) -> Self {
        Self::Serialization(error)
    }
}

/// Import schema-v9 data into a new normalized schema-v1 database.
///
/// The source is opened read-only and is never modified. The target must not
/// exist. Only a fully imported and checked staging database is published.
pub fn migrate_legacy_database(
    source: impl AsRef<Path>,
    target: impl AsRef<Path>,
) -> Result<LegacyMigrationSummary, LegacyMigrationError> {
    migrate_legacy(source.as_ref(), target.as_ref(), None)
}

/// Import a legacy database and Notes tree into an atomic server snapshot.
pub fn migrate_legacy_server(
    source: impl AsRef<Path>,
    target: impl AsRef<Path>,
    notes_root: impl AsRef<Path>,
    blob_root: impl AsRef<Path>,
) -> Result<LegacyMigrationSummary, LegacyMigrationError> {
    migrate_legacy(
        source.as_ref(),
        target.as_ref(),
        Some((notes_root.as_ref(), blob_root.as_ref())),
    )
}

fn migrate_legacy(
    source_path: &Path,
    target_path: &Path,
    content: Option<(&Path, &Path)>,
) -> Result<LegacyMigrationSummary, LegacyMigrationError> {
    let paths = MigrationPaths::new(source_path, target_path)?;
    if let Some((_, blob_root)) = content {
        recover_interrupted_publication(&paths, blob_root)?;
    }
    paths.require_available()?;
    reject_emacs_lease(&paths.source)?;
    let source_metadata = fs::metadata(&paths.source)?;
    let source_digest = sha256_file(&paths.source)?;
    let source = open_source(&paths.source)?;
    validate_integrity(&source, LegacyMigrationError::SourceIntegrity)?;
    let version = source_schema_version(&source)?;
    let manifest = legacy_manifest(&source)?;
    require_legacy_schema(&source, &manifest)?;
    reject_active_session(&source)?;

    let mut staging = StagingDatabase::create(paths.staging)?;
    import_native_snapshot(
        &source,
        staging.path(),
        version,
        &manifest,
        &source_digest,
        &source_metadata,
    )?;
    let mut content_summary = content
        .map(|(notes_root, blob_root)| {
            import_content_snapshot(
                &source,
                &paths.source,
                staging.path(),
                notes_root,
                blob_root,
            )
        })
        .transpose()?;
    publish_snapshot(&mut staging, &paths.target, content_summary.as_mut())?;
    migration_summary(version, source_digest, &manifest, content_summary)
}

struct MigrationPaths {
    source: PathBuf,
    target: PathBuf,
    staging: PathBuf,
}

impl MigrationPaths {
    fn new(source: &Path, target: &Path) -> Result<Self, LegacyMigrationError> {
        let source = fs::canonicalize(source).map_err(|_| LegacyMigrationError::InvalidSource)?;
        if !fs::metadata(&source)?.is_file() {
            return Err(LegacyMigrationError::InvalidSource);
        }
        let target = canonical_destination(target)?;
        if source == target {
            return Err(LegacyMigrationError::SameDatabase);
        }
        let staging = staging_path(&target)?;
        Ok(Self {
            source,
            target,
            staging,
        })
    }

    fn require_available(&self) -> Result<(), LegacyMigrationError> {
        if exists_without_following(&self.target)? {
            return Err(LegacyMigrationError::TargetExists);
        }
        if exists_without_following(&self.staging)? {
            return Err(LegacyMigrationError::StagingExists);
        }
        Ok(())
    }
}

fn canonical_destination(path: &Path) -> Result<PathBuf, LegacyMigrationError> {
    let name = path.file_name().ok_or(LegacyMigrationError::SameDatabase)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    Ok(fs::canonicalize(parent)?.join(name))
}

fn staging_path(target: &Path) -> Result<PathBuf, LegacyMigrationError> {
    let name = target
        .file_name()
        .ok_or(LegacyMigrationError::SameDatabase)?
        .to_string_lossy();
    Ok(target.with_file_name(format!(".{name}.migrating")))
}

fn exists_without_following(path: &Path) -> Result<bool, LegacyMigrationError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn reject_emacs_lease(source: &Path) -> Result<(), LegacyMigrationError> {
    let name = source
        .file_name()
        .ok_or(LegacyMigrationError::InvalidSource)?
        .to_string_lossy();
    let lease = source.with_file_name(format!(".#{name}.gnosis-lease"));
    if exists_without_following(&lease)? {
        return Err(LegacyMigrationError::SourceLeased);
    }
    Ok(())
}

fn open_source(path: &Path) -> Result<Connection, LegacyMigrationError> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.pragma_update(None, "query_only", true)?;
    connection.busy_timeout(Duration::from_secs(5))?;
    Ok(connection)
}

fn source_schema_version(connection: &Connection) -> Result<u32, LegacyMigrationError> {
    let found: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let found = u32::try_from(found).map_err(|_| LegacyMigrationError::SourceIntegrity)?;
    if found != LEGACY_SCHEMA_VERSION {
        return Err(LegacyMigrationError::UnsupportedSourceSchema { found });
    }
    Ok(found)
}

fn legacy_manifest(connection: &Connection) -> Result<BTreeMap<String, u64>, LegacyMigrationError> {
    let mut statement = connection.prepare(
        "SELECT name FROM sqlite_schema
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )?;
    let names = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    names
        .into_iter()
        .map(|name| table_manifest_entry(connection, name))
        .collect()
}

fn table_manifest_entry(
    connection: &Connection,
    name: String,
) -> Result<(String, u64), LegacyMigrationError> {
    let identifier = quoted_identifier(&name);
    let count: i64 =
        connection.query_row(&format!("SELECT count(*) FROM {identifier}"), [], |row| {
            row.get(0)
        })?;
    let count = u64::try_from(count).map_err(|_| LegacyMigrationError::SourceIntegrity)?;
    Ok((name, count))
}

fn quoted_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn require_legacy_schema(
    connection: &Connection,
    manifest: &BTreeMap<String, u64>,
) -> Result<(), LegacyMigrationError> {
    for (table, columns) in REQUIRED_LEGACY_SCHEMA {
        if !manifest.contains_key(*table) || !has_columns(connection, table, columns)? {
            return Err(LegacyMigrationError::IncompleteSourceSchema);
        }
    }
    Ok(())
}

const REQUIRED_LEGACY_SCHEMA: &[(&str, &[&str])] = &[
    (
        "themata",
        &[
            "id",
            "type",
            "keimenon",
            "hypothesis",
            "answer",
            "source_guid",
            "archived_at_us",
        ],
    ),
    ("extras", &["id", "parathema", "review_image"]),
    ("thema_tag", &["thema_id", "tag"]),
    ("thema_links", &["source", "dest"]),
    ("nodes", &["id", "file", "title", "level"]),
    ("node_tag", &["node_id", "tag"]),
    (
        "review_state",
        &[
            "thema_id",
            "state_revision",
            "review_count",
            "legacy_review_count",
            "legacy_unclassified_count",
            "last_reviewed_at_us",
            "last_review_day",
            "next_review_day",
            "interval_days",
            "consecutive_successes",
            "total_successes",
            "consecutive_failures",
            "total_failures",
            "suspended",
            "gnosis_plus",
            "gnosis_minus",
            "gnosis_synolon",
        ],
    ),
];

fn has_columns(
    connection: &Connection,
    table: &str,
    required: &[&str],
) -> Result<bool, LegacyMigrationError> {
    let mut statement = connection.prepare("SELECT name FROM pragma_table_info(?1)")?;
    let columns = statement
        .query_map([table], |row| row.get::<_, String>(0))?
        .collect::<Result<BTreeSet<_>, _>>()?;
    Ok(required.iter().all(|name| columns.contains(*name)))
}

fn reject_active_session(connection: &Connection) -> Result<(), LegacyMigrationError> {
    if !table_exists(connection, "review_session")? {
        return Ok(());
    }
    let active: i64 = connection.query_row(
        "SELECT count(*) FROM review_session WHERE status = 'active'",
        [],
        |row| row.get(0),
    )?;
    if active == 0 {
        Ok(())
    } else {
        Err(LegacyMigrationError::ActiveReviewSession)
    }
}

fn table_exists(connection: &Connection, table: &str) -> rusqlite::Result<bool> {
    connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1)",
        [table],
        |row| row.get(0),
    )
}

fn import_native_snapshot(
    source: &Connection,
    staging: &Path,
    version: u32,
    manifest: &BTreeMap<String, u64>,
    source_digest: &str,
    source_metadata: &fs::Metadata,
) -> Result<(), LegacyMigrationError> {
    let mut target = Connection::open(staging)?;
    initialize_native_database(&target).map_err(|_| LegacyMigrationError::TargetIntegrity)?;
    let imported_at_us = now_us()?;
    let transaction = target.transaction_with_behavior(TransactionBehavior::Immediate)?;
    import_themata(source, &transaction, imported_at_us)?;
    import_review_state(source, &transaction)?;
    import_thema_tags(source, &transaction)?;
    import_thema_links(source, &transaction)?;
    import_review_sessions(source, &transaction)?;
    import_review_events(source, &transaction)?;
    import_review_days(source, &transaction)?;
    record_manifest(
        &transaction,
        version,
        manifest,
        source_digest,
        source_metadata,
        imported_at_us,
    )?;
    transaction.commit()?;
    validate_native_counts(&target, manifest)?;
    validate_integrity(&target, LegacyMigrationError::TargetIntegrity)
}

fn import_themata(
    source: &Connection,
    target: &Transaction<'_>,
    imported_at_us: i64,
) -> Result<(), LegacyMigrationError> {
    let mut query = source.prepare(
        "SELECT thema.id, thema.type, thema.keimenon, thema.hypothesis,
                thema.answer, thema.source_guid, thema.archived_at_us,
                extras.parathema, extras.review_image
         FROM themata AS thema LEFT JOIN extras ON extras.id = thema.id
         ORDER BY thema.id",
    )?;
    let mut rows = query.query([])?;
    let mut insert = target.prepare_cached(
        "INSERT INTO thema(
             thema_id, legacy_id, revision, kind, keimenon, hypothesis_json,
             answer_json, source_guid, archived_at_us, parathema, review_image,
             content_sha256, created_at_us, updated_at_us
         ) VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)",
    )?;
    while let Some(row) = rows.next()? {
        let legacy_id: i64 = row.get(0)?;
        let kind = decode_string(&sql_text(row, 1)?, "thema type")?;
        if !matches!(kind.as_str(), "basic" | "cloze" | "mcq" | "mc-cloze") {
            return Err(LegacyMigrationError::InvalidLegacyValue {
                field: "thema type",
            });
        }
        let keimenon = decode_string(&sql_text(row, 2)?, "keimenon")?;
        let hypothesis = decode_optional_list(&sql_text(row, 3)?, "hypothesis")?;
        let answer = decode_string_list(&sql_text(row, 4)?, "answer")?;
        let source_guid = decode_optional_opaque(row.get(5)?, "source GUID")?;
        let archived_at_us: Option<i64> = row.get(6)?;
        let parathema = decode_optional(row.get(7)?, "parathema")?;
        let review_image =
            decode_optional(row.get(8)?, "review image")?.filter(|value| !value.is_empty());
        let hypothesis_json = serde_json::to_string(&hypothesis)?;
        let answer_json = serde_json::to_string(&answer)?;
        let digest = thema_digest(
            &kind,
            &keimenon,
            &hypothesis,
            &answer,
            source_guid.as_deref(),
            parathema.as_deref(),
            review_image.as_deref(),
        )?;
        insert.execute(params![
            legacy_id.to_string(),
            legacy_id,
            kind,
            keimenon,
            hypothesis_json,
            answer_json,
            source_guid,
            archived_at_us,
            parathema,
            review_image,
            digest,
            imported_at_us,
        ])?;
    }
    Ok(())
}

fn decode_string(value: &str, field: &'static str) -> Result<String, LegacyMigrationError> {
    legacy_value::string(value, field)
        .map_err(|_| LegacyMigrationError::InvalidLegacyValue { field })
}

fn sql_text(row: &rusqlite::Row<'_>, index: usize) -> Result<String, LegacyMigrationError> {
    row.get_ref(index)?
        .as_str()
        .map(ToOwned::to_owned)
        .map_err(|_| LegacyMigrationError::SourceIntegrity)
}

fn decode_optional(
    value: Option<String>,
    field: &'static str,
) -> Result<Option<String>, LegacyMigrationError> {
    legacy_value::optional_string(value.as_deref(), field)
        .map_err(|_| LegacyMigrationError::InvalidLegacyValue { field })
}

fn decode_optional_opaque(
    value: Option<String>,
    field: &'static str,
) -> Result<Option<String>, LegacyMigrationError> {
    match value.as_deref() {
        None | Some("nil") => Ok(None),
        Some(value) if value.starts_with('"') => decode_optional(Some(value.to_owned()), field),
        Some(value) if !value.is_empty() => Ok(Some(value.to_owned())),
        Some(_) => Err(LegacyMigrationError::InvalidLegacyValue { field }),
    }
}

fn decode_optional_list(
    value: &str,
    field: &'static str,
) -> Result<Vec<Option<String>>, LegacyMigrationError> {
    legacy_value::optional_string_list(value, field)
        .map_err(|_| LegacyMigrationError::InvalidLegacyValue { field })
}

fn decode_string_list(
    value: &str,
    field: &'static str,
) -> Result<Vec<String>, LegacyMigrationError> {
    legacy_value::string_list(value, field)
        .map_err(|_| LegacyMigrationError::InvalidLegacyValue { field })
}

fn thema_digest(
    kind: &str,
    keimenon: &str,
    hypothesis: &[Option<String>],
    answer: &[String],
    source_guid: Option<&str>,
    parathema: Option<&str>,
    review_image: Option<&str>,
) -> Result<String, LegacyMigrationError> {
    let canonical = serde_json::to_vec(&json!({
        "answer": answer,
        "hypothesis": hypothesis,
        "keimenon": keimenon,
        "kind": kind,
        "parathema": parathema,
        "review_image": review_image,
        "source_guid": source_guid,
    }))?;
    Ok(format!("{:x}", Sha256::digest(canonical)))
}

fn import_review_state(
    source: &Connection,
    target: &Transaction<'_>,
) -> Result<(), LegacyMigrationError> {
    let mut query = source.prepare(
        "SELECT thema_id, state_revision, review_count, legacy_review_count,
                legacy_unclassified_count, last_reviewed_at_us, last_review_day,
                next_review_day, interval_days, consecutive_successes,
                total_successes, consecutive_failures, total_failures, suspended,
                gnosis_plus, gnosis_minus, gnosis_synolon
         FROM review_state ORDER BY thema_id",
    )?;
    let mut rows = query.query([])?;
    let mut insert = target.prepare_cached(
        "INSERT INTO review_state VALUES (
             ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
             ?14, ?15, ?16, ?17
         )",
    )?;
    while let Some(row) = rows.next()? {
        let mut values = vec![SqlValue::Text(row.get::<_, i64>(0)?.to_string())];
        for index in 1..=16 {
            values.push(row.get(index)?);
        }
        insert.execute(params_from_iter(values))?;
    }
    Ok(())
}

fn import_thema_tags(
    source: &Connection,
    target: &Transaction<'_>,
) -> Result<(), LegacyMigrationError> {
    let mut query = source.prepare("SELECT thema_id, tag FROM thema_tag ORDER BY thema_id, tag")?;
    let mut rows = query.query([])?;
    let mut insert =
        target.prepare_cached("INSERT INTO thema_tag(thema_id, tag) VALUES (?1, ?2)")?;
    while let Some(row) = rows.next()? {
        let tag = decode_string(&sql_text(row, 1)?, "thema tag")?;
        insert.execute(params![row.get::<_, i64>(0)?.to_string(), tag])?;
    }
    Ok(())
}

fn import_thema_links(
    source: &Connection,
    target: &Transaction<'_>,
) -> Result<(), LegacyMigrationError> {
    let mut query = source.prepare("SELECT source, dest FROM thema_links ORDER BY source, dest")?;
    let mut rows = query.query([])?;
    let mut insert =
        target.prepare_cached("INSERT INTO thema_node_link(thema_id, node_id) VALUES (?1, ?2)")?;
    while let Some(row) = rows.next()? {
        let source_id = sql_text(row, 0)?;
        let target_id = decode_string(&sql_text(row, 1)?, "linked node ID")?;
        insert.execute(params![source_id, target_id])?;
    }
    Ok(())
}

fn import_review_sessions(
    source: &Connection,
    target: &Transaction<'_>,
) -> Result<(), LegacyMigrationError> {
    if !table_exists(source, "review_session")? {
        return Ok(());
    }
    let mut query = source.prepare("SELECT * FROM review_session ORDER BY started_at_us")?;
    let columns = query
        .column_names()
        .into_iter()
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    let mut rows = query.query([])?;
    let scheduler_json: String = target.query_row(
        "SELECT scheduler_json FROM settings WHERE id = 1",
        [],
        |row| row.get(0),
    )?;
    let mut insert = target.prepare_cached(
        "INSERT INTO review_session(
             session_id, source_kind, source_json, target_count, timezone,
             day_boundary_hour, scheduler_json, status, started_at_us,
             ended_at_us, current_position
         ) VALUES (?1, ?2, ?3, ?4, 'UTC', ?5, ?6, ?7, ?8, ?9, ?10)",
    )?;
    while let Some(row) = rows.next()? {
        let payload = row_json(row, &columns)?;
        insert.execute(params![
            json_string(&payload, "session_id")?,
            json_string(&payload, "source_kind")?,
            serde_json::to_string(&payload)?,
            json_optional_i64(&payload, "planned_count")?,
            json_i64(&payload, "day_start_hour")?,
            scheduler_json,
            json_string(&payload, "status")?,
            json_i64(&payload, "started_at_us")?,
            json_optional_i64(&payload, "ended_at_us")?,
            json_i64(&payload, "event_count")?,
        ])?;
    }
    Ok(())
}

fn import_review_events(
    source: &Connection,
    target: &Transaction<'_>,
) -> Result<(), LegacyMigrationError> {
    if !table_exists(source, "review_event")? {
        return Ok(());
    }
    let mut query =
        source.prepare("SELECT * FROM review_event ORDER BY reviewed_at_us, event_id")?;
    let columns = query
        .column_names()
        .into_iter()
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    let mut rows = query.query([])?;
    let mut insert = target.prepare_cached(
        "INSERT INTO review_event(
             event_id, thema_id, session_id, reviewed_at_us, review_day,
             matcher_version, response_json, detected_success, accepted_success,
             override_count, scheduler_version, transition_json, imported_payload_json
         ) VALUES (?1, ?2, ?3, ?4, ?5, 1, NULL, ?6, ?7, ?8, ?9, ?10, ?11)",
    )?;
    while let Some(row) = rows.next()? {
        let mut payload = row_json(row, &columns)?;
        let event_id = json_string(&payload, "event_id")?.to_owned();
        payload.insert(
            "proto".into(),
            event_children(source, "review_event_proto", &event_id)?,
        );
        payload.insert(
            "nodes".into(),
            event_children(source, "review_event_node", &event_id)?,
        );
        let payload_json = serde_json::to_string(&payload)?;
        insert.execute(params![
            event_id,
            json_i64(&payload, "thema_id")?.to_string(),
            json_string(&payload, "session_id")?,
            json_i64(&payload, "reviewed_at_us")?,
            json_i64(&payload, "review_day")?,
            outcome_value(json_string(&payload, "detected_outcome")?)?,
            outcome_value(json_string(&payload, "accepted_outcome")?)?,
            json_i64(&payload, "override_count")?,
            json_i64(&payload, "scheduler_version")?,
            payload_json,
            payload_json,
        ])?;
    }
    Ok(())
}

fn event_children(
    source: &Connection,
    table: &str,
    event_id: &str,
) -> Result<Value, LegacyMigrationError> {
    if !table_exists(source, table)? {
        return Ok(Value::Array(Vec::new()));
    }
    let mut query = source.prepare(&format!(
        "SELECT * FROM {} WHERE event_id = ?1 ORDER BY 1, 2",
        quoted_identifier(table)
    ))?;
    let columns = query
        .column_names()
        .into_iter()
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    let rows = query
        .query_map([event_id], |row| {
            row_json_sql(row, &columns).map(Value::Object)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Value::Array(rows))
}

fn import_review_days(
    source: &Connection,
    target: &Transaction<'_>,
) -> Result<(), LegacyMigrationError> {
    if !table_exists(source, "review_day")? {
        return Ok(());
    }
    let mut query = source.prepare(
        "SELECT review_day,
                legacy_review_count + exact_review_count,
                coalesce(legacy_new_count, 0) + exact_new_count,
                exact_success_count, exact_failure_count, exact_override_count
         FROM review_day ORDER BY review_day",
    )?;
    let mut rows = query.query([])?;
    let mut insert =
        target.prepare_cached("INSERT INTO review_day VALUES (?1, ?2, ?3, ?4, ?5, ?6)")?;
    while let Some(row) = rows.next()? {
        let values = (0..6)
            .map(|index| row.get::<_, SqlValue>(index))
            .collect::<Result<Vec<_>, _>>()?;
        insert.execute(params_from_iter(values))?;
    }
    Ok(())
}

fn record_manifest(
    target: &Transaction<'_>,
    version: u32,
    manifest: &BTreeMap<String, u64>,
    source_digest: &str,
    source_metadata: &fs::Metadata,
    imported_at_us: i64,
) -> Result<(), LegacyMigrationError> {
    let source_size =
        i64::try_from(source_metadata.len()).map_err(|_| LegacyMigrationError::SourceIntegrity)?;
    let mtime_ns = source_metadata
        .mtime()
        .checked_mul(1_000_000_000)
        .and_then(|value| value.checked_add(source_metadata.mtime_nsec()))
        .ok_or(LegacyMigrationError::SourceIntegrity)?;
    target.execute(
        "INSERT INTO import_manifest VALUES (1, ?1, ?2, ?3, ?4, ?5)",
        params![
            version,
            source_digest,
            source_size,
            mtime_ns,
            imported_at_us
        ],
    )?;
    let mut insert = target.prepare_cached(
        "INSERT INTO import_table_count(table_name, source_rows, imported_rows)
         VALUES (?1, ?2, ?2)",
    )?;
    for (table, rows) in manifest {
        insert.execute(params![
            table,
            i64::try_from(*rows).map_err(|_| LegacyMigrationError::SourceIntegrity)?
        ])?;
    }
    Ok(())
}

fn validate_native_counts(
    target: &Connection,
    manifest: &BTreeMap<String, u64>,
) -> Result<(), LegacyMigrationError> {
    for (source_table, target_table) in [
        ("themata", "thema"),
        ("review_state", "review_state"),
        ("thema_tag", "thema_tag"),
        ("thema_links", "thema_node_link"),
    ] {
        let expected = manifest
            .get(source_table)
            .ok_or(LegacyMigrationError::ManifestMismatch)?;
        let actual: i64 = target.query_row(
            &format!("SELECT count(*) FROM {}", quoted_identifier(target_table)),
            [],
            |row| row.get(0),
        )?;
        if u64::try_from(actual).ok() != Some(*expected) {
            return Err(LegacyMigrationError::ManifestMismatch);
        }
    }
    Ok(())
}

fn row_json(
    row: &rusqlite::Row<'_>,
    columns: &[String],
) -> Result<Map<String, Value>, LegacyMigrationError> {
    row_json_sql(row, columns).map_err(Into::into)
}

fn row_json_sql(
    row: &rusqlite::Row<'_>,
    columns: &[String],
) -> rusqlite::Result<Map<String, Value>> {
    columns
        .iter()
        .enumerate()
        .map(|(index, name)| Ok((name.clone(), value_json(row.get_ref(index)?))))
        .collect()
}

fn value_json(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(value) => Value::from(value),
        ValueRef::Real(value) => Value::from(value),
        ValueRef::Text(value) => Value::String(String::from_utf8_lossy(value).into_owned()),
        ValueRef::Blob(value) => Value::String(hex_bytes(value)),
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn json_string<'a>(
    object: &'a Map<String, Value>,
    key: &str,
) -> Result<&'a str, LegacyMigrationError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or(LegacyMigrationError::SourceIntegrity)
}

fn json_i64(object: &Map<String, Value>, key: &str) -> Result<i64, LegacyMigrationError> {
    object
        .get(key)
        .and_then(Value::as_i64)
        .ok_or(LegacyMigrationError::SourceIntegrity)
}

fn json_optional_i64(
    object: &Map<String, Value>,
    key: &str,
) -> Result<Option<i64>, LegacyMigrationError> {
    match object.get(key) {
        Some(Value::Null) | None => Ok(None),
        Some(value) => value
            .as_i64()
            .map(Some)
            .ok_or(LegacyMigrationError::SourceIntegrity),
    }
}

fn outcome_value(value: &str) -> Result<i64, LegacyMigrationError> {
    match value {
        "success" => Ok(1),
        "failure" => Ok(0),
        _ => Err(LegacyMigrationError::SourceIntegrity),
    }
}

fn sha256_file(path: &Path) -> Result<String, LegacyMigrationError> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

struct ContentImport {
    note_count: u64,
    asset_count: u64,
    blobs: StagingBlobDirectory,
}

fn import_content_snapshot(
    source: &Connection,
    source_database: &Path,
    staging_database: &Path,
    notes_root: &Path,
    blob_root: &Path,
) -> Result<ContentImport, LegacyMigrationError> {
    let notes_root =
        fs::canonicalize(notes_root).map_err(|_| LegacyMigrationError::InvalidNotesTree)?;
    if !fs::metadata(&notes_root)?.is_dir() {
        return Err(LegacyMigrationError::InvalidNotesTree);
    }
    let blobs = StagingBlobDirectory::create(blob_root)?;
    let blob_store =
        BlobStore::open(blobs.staging.clone()).map_err(|_| LegacyMigrationError::Blob)?;
    let mut store =
        SqliteStore::open(staging_database).map_err(|_| LegacyMigrationError::Storage)?;
    let imported_at_us = now_us()?;
    let files = collect_notes_files(&notes_root)?;
    let mut asset_map = import_tree_assets(
        &mut store,
        &blob_store,
        &notes_root,
        &files.assets,
        "notes",
        imported_at_us,
    )?;
    let mut imported_asset_paths = asset_map.values().cloned().collect::<BTreeSet<_>>();
    let legacy_assets = source_database
        .parent()
        .ok_or(LegacyMigrationError::InvalidSource)?
        .join("images");
    let legacy_map = if legacy_assets.is_dir() {
        let legacy_files = collect_asset_files(&legacy_assets)?;
        import_tree_assets(
            &mut store,
            &blob_store,
            &legacy_assets,
            &legacy_files,
            "legacy-images",
            imported_at_us,
        )?
    } else {
        BTreeMap::new()
    };
    normalize_thema_assets(source, &mut store, &legacy_assets, &legacy_map)?;
    imported_asset_paths.extend(legacy_map.values().cloned());
    for (source_path, asset_path) in legacy_map {
        asset_map.entry(source_path).or_insert(asset_path);
    }
    let note_paths = validated_note_paths(&notes_root, &files.org)?;
    for path in &files.org {
        import_note(
            &mut store,
            &blob_store,
            &notes_root,
            path,
            &note_paths,
            &asset_map,
            imported_at_us,
        )?;
    }
    Ok(ContentImport {
        note_count: files.org.len() as u64,
        asset_count: imported_asset_paths.len() as u64,
        blobs,
    })
}

struct NotesFiles {
    org: Vec<PathBuf>,
    assets: Vec<PathBuf>,
}

fn collect_notes_files(root: &Path) -> Result<NotesFiles, LegacyMigrationError> {
    let mut org = Vec::new();
    let mut assets = Vec::new();
    collect_files(root, root, &mut org, &mut assets)?;
    org.sort();
    assets.sort();
    Ok(NotesFiles { org, assets })
}

fn collect_files(
    root: &Path,
    directory: &Path,
    org: &mut Vec<PathBuf>,
    assets: &mut Vec<PathBuf>,
) -> Result<(), LegacyMigrationError> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_name() == ".git" {
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(LegacyMigrationError::InvalidNotesTree);
        }
        if metadata.is_dir() {
            collect_files(root, &path, org, assets)?;
        } else if metadata.is_file() {
            classify_file(root, path, org, assets)?;
        }
    }
    Ok(())
}

fn classify_file(
    root: &Path,
    path: PathBuf,
    org: &mut Vec<PathBuf>,
    assets: &mut Vec<PathBuf>,
) -> Result<(), LegacyMigrationError> {
    let relative = relative_utf8(root, &path)?;
    if relative.ends_with(".org") && !relative.starts_with("assets/") {
        org.push(path);
    } else if supported_asset(&relative) {
        assets.push(path);
    } else if relative.starts_with("assets/") {
        return Err(LegacyMigrationError::InvalidNotesTree);
    }
    Ok(())
}

fn collect_asset_files(root: &Path) -> Result<Vec<PathBuf>, LegacyMigrationError> {
    let files = collect_notes_files(root)?;
    Ok(files.assets)
}

fn validated_note_paths(
    root: &Path,
    paths: &[PathBuf],
) -> Result<BTreeMap<String, String>, LegacyMigrationError> {
    let mut keys = BTreeSet::new();
    let mut validated = BTreeMap::new();
    for path in paths {
        let source = relative_utf8(root, path)?;
        if !keys.insert(path_key(&source)) {
            return Err(LegacyMigrationError::InvalidNotesTree);
        }
        validated.insert(source.clone(), source);
    }
    Ok(validated)
}

fn import_tree_assets(
    store: &mut SqliteStore,
    blobs: &BlobStore,
    root: &Path,
    files: &[PathBuf],
    source_prefix: &str,
    imported_at_us: i64,
) -> Result<BTreeMap<String, String>, LegacyMigrationError> {
    let mut mapping = BTreeMap::new();
    for path in files {
        let relative = relative_utf8(root, path)?;
        let bytes = fs::read(path)?;
        let media_type = media_type(path).ok_or(LegacyMigrationError::InvalidNotesTree)?;
        let blob = blobs
            .put(&bytes, media_type)
            .map_err(|_| LegacyMigrationError::Blob)?;
        let original_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(LegacyMigrationError::InvalidNotesTree)?;
        let asset_path = canonical_asset_path(&blob.sha256, original_name)
            .map_err(|_| LegacyMigrationError::InvalidNotesTree)?;
        let prepared = prepare_asset(asset_path.clone(), original_name.to_owned(), None)
            .map_err(|_| LegacyMigrationError::InvalidNotesTree)?;
        store
            .put_asset(&prepared, &blob, imported_at_us, None)
            .map_err(|_| LegacyMigrationError::Storage)?;
        store
            .record_import_path_rewrite(
                &format!("{source_prefix}/{relative}"),
                &asset_path,
                "asset_normalization",
            )
            .map_err(|_| LegacyMigrationError::Storage)?;
        mapping.insert(relative, asset_path);
    }
    Ok(mapping)
}

fn normalize_thema_assets(
    source: &Connection,
    store: &mut SqliteStore,
    root: &Path,
    mapping: &BTreeMap<String, String>,
) -> Result<(), LegacyMigrationError> {
    let mut statement = source.prepare("SELECT id FROM themata ORDER BY id")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let id = row.get::<_, i64>(0)?.to_string();
        let mut record = store
            .get_thema(&id)
            .map_err(|_| LegacyMigrationError::Storage)?
            .ok_or(LegacyMigrationError::SourceIntegrity)?;
        let mut assets = record.asset_paths.iter().cloned().collect::<BTreeSet<_>>();
        let mut changed =
            rewrite_thema_text(&mut record.content.keimenon, root, mapping, &mut assets)?;
        for value in record.content.hypothesis.iter_mut().flatten() {
            changed |= rewrite_thema_text(value, root, mapping, &mut assets)?;
        }
        for value in &mut record.content.answer {
            changed |= rewrite_thema_text(value, root, mapping, &mut assets)?;
        }
        if let Some(value) = record.content.parathema.as_mut() {
            changed |= rewrite_thema_text(value, root, mapping, &mut assets)?;
        }
        if let Some(reference) = record.content.review_image.as_mut()
            && !reference.is_empty()
            && !external_reference(reference)
        {
            let asset_path = mapped_legacy_asset(root, reference, mapping)?.to_owned();
            changed |= *reference != asset_path;
            *reference = asset_path.clone();
            assets.insert(asset_path);
        }
        if changed {
            record.asset_paths = assets.into_iter().collect();
            record.content_sha256 = migrated_thema_digest(&record)?;
            store
                .rewrite_imported_thema_content(&record)
                .map_err(|_| LegacyMigrationError::Storage)?;
        }
    }
    Ok(())
}

fn rewrite_thema_text(
    value: &mut String,
    root: &Path,
    mapping: &BTreeMap<String, String>,
    assets: &mut BTreeSet<String>,
) -> Result<bool, LegacyMigrationError> {
    let mut replacements = Vec::new();
    for expression in crate::application::file_link_expressions() {
        for captures in expression.captures_iter(value.as_str()) {
            let capture = captures
                .get(1)
                .ok_or(LegacyMigrationError::InvalidNotesTree)?;
            let (target, search) = capture
                .as_str()
                .split_once("::")
                .map_or((capture.as_str(), ""), |(target, search)| (target, search));
            if external_reference(target) {
                continue;
            }
            let asset_path = mapped_legacy_asset(root, target, mapping)?.to_owned();
            assets.insert(asset_path.clone());
            let replacement = if search.is_empty() {
                asset_path
            } else {
                format!("{asset_path}::{search}")
            };
            replacements.push((capture.start(), capture.end(), replacement));
        }
    }
    let changed = !replacements.is_empty();
    for (start, end, replacement) in replacements.into_iter().rev() {
        value.replace_range(start..end, &replacement);
    }
    Ok(changed)
}

fn mapped_legacy_asset<'a>(
    root: &Path,
    reference: &str,
    mapping: &'a BTreeMap<String, String>,
) -> Result<&'a String, LegacyMigrationError> {
    let root = fs::canonicalize(root).map_err(|_| LegacyMigrationError::InvalidNotesTree)?;
    let candidate = if Path::new(reference).is_absolute() {
        fs::canonicalize(reference).map_err(|_| LegacyMigrationError::InvalidNotesTree)?
    } else {
        let reference = reference.strip_prefix("images/").unwrap_or(reference);
        let normalized = normalize_reference(Path::new(""), reference)?;
        fs::canonicalize(root.join(normalized))
            .map_err(|_| LegacyMigrationError::InvalidNotesTree)?
    };
    let relative = relative_utf8(&root, &candidate)?;
    mapping
        .get(&relative)
        .ok_or(LegacyMigrationError::InvalidNotesTree)
}

fn migrated_thema_digest(
    record: &crate::application::ThemaRecord,
) -> Result<String, LegacyMigrationError> {
    let content = &record.content;
    let canonical = serde_json::to_vec(&json!({
        "answer": content.answer,
        "assets": record.asset_paths,
        "hypothesis": content.hypothesis,
        "keimenon": content.keimenon,
        "kind": content.kind.as_str(),
        "nodes": record.node_ids,
        "parathema": content.parathema,
        "review_image": content.review_image,
        "source_guid": content.source_guid,
        "tags": record.tags,
    }))?;
    Ok(format!("{:x}", Sha256::digest(canonical)))
}

fn external_reference(value: &str) -> bool {
    value.starts_with("http://") || value.starts_with("https://")
}

fn import_note(
    store: &mut SqliteStore,
    blobs: &BlobStore,
    root: &Path,
    path: &Path,
    note_paths: &BTreeMap<String, String>,
    asset_map: &BTreeMap<String, String>,
    imported_at_us: i64,
) -> Result<(), LegacyMigrationError> {
    let source_path = relative_utf8(root, path)?;
    let logical_path = note_paths
        .get(&source_path)
        .ok_or(LegacyMigrationError::InvalidNotesTree)?
        .clone();
    let org = fs::read_to_string(path)?;
    let org = rewrite_file_links(&source_path, &org, note_paths, asset_map)?;
    let prepared = prepare_note(None, logical_path, org, None)
        .map_err(|_| LegacyMigrationError::InvalidOrg { path: source_path })?;
    let blob = blobs
        .put(prepared.org.as_bytes(), "text/org; charset=utf-8")
        .map_err(|_| LegacyMigrationError::Blob)?;
    store
        .replace_note(&prepared, &blob, imported_at_us, None)
        .map_err(|_| LegacyMigrationError::Storage)?;
    Ok(())
}

fn rewrite_file_links(
    document_path: &str,
    org: &str,
    note_paths: &BTreeMap<String, String>,
    asset_map: &BTreeMap<String, String>,
) -> Result<String, LegacyMigrationError> {
    let parent = Path::new(document_path)
        .parent()
        .unwrap_or_else(|| Path::new(""));
    let mut replacements = Vec::new();
    for expression in crate::application::file_link_expressions() {
        for captures in expression.captures_iter(org) {
            let capture = captures
                .get(1)
                .ok_or(LegacyMigrationError::InvalidNotesTree)?;
            let (target, search) = capture
                .as_str()
                .split_once("::")
                .map_or((capture.as_str(), ""), |(target, search)| (target, search));
            if target.starts_with("http://") || target.starts_with("https://") {
                continue;
            }
            let resolved = normalize_reference(parent, target)?;
            let replacement = note_paths
                .get(&resolved)
                .or_else(|| asset_map.get(&resolved))
                .ok_or(LegacyMigrationError::InvalidNotesTree)?;
            let replacement = if search.is_empty() {
                replacement.clone()
            } else {
                format!("{replacement}::{search}")
            };
            replacements.push((capture.start(), capture.end(), replacement));
        }
    }
    let mut rewritten = org.to_owned();
    for (start, end, replacement) in replacements.into_iter().rev() {
        rewritten.replace_range(start..end, &replacement);
    }
    Ok(rewritten)
}

fn normalize_reference(parent: &Path, target: &str) -> Result<String, LegacyMigrationError> {
    if Path::new(target).is_absolute() || target.contains('\\') {
        return Err(LegacyMigrationError::InvalidNotesTree);
    }
    let mut parts = parent
        .components()
        .filter_map(|part| match part {
            std::path::Component::Normal(value) => value.to_str().map(ToOwned::to_owned),
            _ => None,
        })
        .collect::<Vec<_>>();
    for component in Path::new(target).components() {
        match component {
            std::path::Component::Normal(value) => parts.push(
                value
                    .to_str()
                    .ok_or(LegacyMigrationError::InvalidNotesTree)?
                    .to_owned(),
            ),
            std::path::Component::ParentDir if parts.pop().is_some() => {}
            _ => return Err(LegacyMigrationError::InvalidNotesTree),
        }
    }
    let value = parts.join("/");
    validate_logical_path(&value).map_err(|_| LegacyMigrationError::InvalidNotesTree)?;
    Ok(value)
}

fn relative_utf8(root: &Path, path: &Path) -> Result<String, LegacyMigrationError> {
    path.strip_prefix(root)
        .ok()
        .and_then(Path::to_str)
        .map(|value| value.replace(std::path::MAIN_SEPARATOR, "/"))
        .ok_or(LegacyMigrationError::InvalidNotesTree)
}

fn media_type(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "svg" => Some("image/svg+xml"),
        "pdf" => Some("application/pdf"),
        "mp3" => Some("audio/mpeg"),
        "ogg" => Some("audio/ogg"),
        "wav" => Some("audio/wav"),
        "mp4" => Some("video/mp4"),
        _ => None,
    }
}

struct StagingBlobDirectory {
    staging: PathBuf,
    target: PathBuf,
    published: bool,
}

impl StagingBlobDirectory {
    fn create(target: &Path) -> Result<Self, LegacyMigrationError> {
        let target = canonical_destination(target)?;
        if exists_without_following(&target)? {
            return Err(LegacyMigrationError::TargetExists);
        }
        let staging = staging_path(&target)?;
        if exists_without_following(&staging)? {
            return Err(LegacyMigrationError::StagingExists);
        }
        Ok(Self {
            staging,
            target,
            published: false,
        })
    }

    fn publish(&mut self) -> Result<(), LegacyMigrationError> {
        fs::rename(&self.staging, &self.target)?;
        sync_parent_directory(&self.target)?;
        self.published = true;
        Ok(())
    }
}

impl Drop for StagingBlobDirectory {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_dir_all(&self.staging);
        }
    }
}

fn publish_snapshot(
    database: &mut StagingDatabase,
    target: &Path,
    content: Option<&mut ContentImport>,
) -> Result<(), LegacyMigrationError> {
    if let Some(content) = content {
        let mut publication = PublicationMarker::create(target, &content.blobs.target)?;
        content.blobs.publish()?;
        if let Err(error) = database.publish(target) {
            preserve_publication_if_target_exists(&mut publication, target);
            return Err(error);
        }
        publication.complete()?;
        return Ok(());
    }
    database.publish(target)
}

struct PublicationMarker {
    path: PathBuf,
    blob_target: PathBuf,
    cleanup_on_drop: bool,
}

impl PublicationMarker {
    fn create(database_target: &Path, blob_target: &Path) -> Result<Self, LegacyMigrationError> {
        let path = publication_marker_path(database_target)?;
        let mut marker = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        marker.write_all(&publication_marker_contents(database_target, blob_target))?;
        marker.sync_all()?;
        sync_parent_directory(&path)?;
        Ok(Self {
            path,
            blob_target: blob_target.to_owned(),
            cleanup_on_drop: true,
        })
    }

    fn complete(&mut self) -> Result<(), LegacyMigrationError> {
        self.cleanup_on_drop = false;
        fs::remove_file(&self.path)?;
        sync_parent_directory(&self.path)
    }

    fn preserve(&mut self) {
        self.cleanup_on_drop = false;
    }
}

impl Drop for PublicationMarker {
    fn drop(&mut self) {
        if self.cleanup_on_drop {
            let _ = remove_directory_if_exists(&self.blob_target);
            let _ = fs::remove_file(&self.path);
            let _ = sync_parent_directory(&self.path);
        }
    }
}

fn preserve_publication_if_target_exists(publication: &mut PublicationMarker, target: &Path) {
    if !matches!(exists_without_following(target), Ok(false)) {
        publication.preserve();
    }
}

fn recover_interrupted_publication(
    paths: &MigrationPaths,
    blob_root: &Path,
) -> Result<(), LegacyMigrationError> {
    let marker = publication_marker_path(&paths.target)?;
    if !exists_without_following(&marker)? {
        return Ok(());
    }
    if !fs::symlink_metadata(&marker)?.file_type().is_file() {
        return Err(LegacyMigrationError::StagingExists);
    }
    let blob_target = canonical_destination(blob_root)?;
    if fs::read(&marker)? != publication_marker_contents(&paths.target, &blob_target) {
        return Err(LegacyMigrationError::PublicationTargetMismatch);
    }
    let blob_staging = staging_path(&blob_target)?;
    if exists_without_following(&paths.target)? {
        remove_directory_if_exists(&blob_staging)?;
        remove_file_if_exists(&paths.staging)?;
        for suffix in ["-journal", "-wal", "-shm"] {
            remove_file_if_exists(&sidecar_path(&paths.staging, suffix))?;
        }
        fs::remove_file(&marker)?;
        sync_parent_directory(&marker)?;
        return Ok(());
    }
    remove_directory_if_exists(&blob_target)?;
    remove_directory_if_exists(&blob_staging)?;
    remove_file_if_exists(&paths.staging)?;
    for suffix in ["-journal", "-wal", "-shm"] {
        remove_file_if_exists(&sidecar_path(&paths.staging, suffix))?;
    }
    fs::remove_file(&marker)?;
    sync_parent_directory(&marker)
}

fn publication_marker_path(database_target: &Path) -> Result<PathBuf, LegacyMigrationError> {
    let name = database_target
        .file_name()
        .ok_or(LegacyMigrationError::SameDatabase)?
        .to_string_lossy();
    Ok(database_target.with_file_name(format!(".{name}.publishing")))
}

fn publication_marker_contents(database_target: &Path, blob_target: &Path) -> Vec<u8> {
    let mut contents = b"gnosis-publication-v1\0".to_vec();
    for path in [database_target, blob_target] {
        let bytes = path.as_os_str().as_bytes();
        contents.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
        contents.extend_from_slice(bytes);
    }
    contents
}

fn remove_directory_if_exists(path: &Path) -> Result<(), LegacyMigrationError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => fs::remove_dir_all(path)?,
        Ok(_) => return Err(LegacyMigrationError::TargetExists),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    sync_parent_directory(path)
}

fn remove_file_if_exists(path: &Path) -> Result<(), LegacyMigrationError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => fs::remove_file(path)?,
        Ok(_) => return Err(LegacyMigrationError::StagingExists),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    sync_parent_directory(path)
}

fn sync_parent_directory(path: &Path) -> Result<(), LegacyMigrationError> {
    File::open(path.parent().ok_or(LegacyMigrationError::SameDatabase)?)?.sync_all()?;
    Ok(())
}

fn now_us() -> Result<i64, LegacyMigrationError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| LegacyMigrationError::SourceIntegrity)?;
    i64::try_from(duration.as_micros()).map_err(|_| LegacyMigrationError::SourceIntegrity)
}

fn validate_integrity(
    connection: &Connection,
    error: LegacyMigrationError,
) -> Result<(), LegacyMigrationError> {
    let mut statement = connection.prepare("PRAGMA quick_check")?;
    let checks = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    if checks.as_slice() != ["ok"] || has_foreign_key_violation(connection)? {
        return Err(error);
    }
    Ok(())
}

fn has_foreign_key_violation(connection: &Connection) -> Result<bool, LegacyMigrationError> {
    let mut statement = connection.prepare("PRAGMA foreign_key_check")?;
    Ok(statement.query([])?.next()?.is_some())
}

fn total_rows(manifest: &BTreeMap<String, u64>) -> Result<u64, LegacyMigrationError> {
    manifest
        .values()
        .try_fold(0_u64, |total, rows| total.checked_add(*rows))
        .ok_or(LegacyMigrationError::SourceIntegrity)
}

fn migration_summary(
    version: u32,
    source_sha256: String,
    manifest: &BTreeMap<String, u64>,
    content: Option<ContentImport>,
) -> Result<LegacyMigrationSummary, LegacyMigrationError> {
    let (note_count, asset_count) = content
        .map(|content| (content.note_count, content.asset_count))
        .unwrap_or((0, 0));
    Ok(LegacyMigrationSummary {
        source_schema_version: version,
        source_sha256,
        legacy_table_count: u64::try_from(manifest.len())
            .map_err(|_| LegacyMigrationError::SourceIntegrity)?,
        legacy_row_count: total_rows(manifest)?,
        thema_count: *manifest
            .get("themata")
            .ok_or(LegacyMigrationError::ManifestMismatch)?,
        note_count,
        asset_count,
    })
}

struct StagingDatabase {
    path: PathBuf,
    published: bool,
}

impl StagingDatabase {
    fn create(path: PathBuf) -> Result<Self, LegacyMigrationError> {
        let result = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path);
        match result {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(LegacyMigrationError::StagingExists);
            }
            Err(error) => return Err(error.into()),
        }
        Ok(Self {
            path,
            published: false,
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn publish(&mut self, target: &Path) -> Result<(), LegacyMigrationError> {
        fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))?;
        match fs::hard_link(&self.path, target) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(LegacyMigrationError::TargetExists);
            }
            Err(error) => return Err(error.into()),
        }
        sync_parent_directory(target)?;
        fs::remove_file(&self.path)?;
        sync_parent_directory(target)?;
        self.published = true;
        Ok(())
    }
}

impl Drop for StagingDatabase {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_file(&self.path);
            let _ = fs::remove_file(sidecar_path(&self.path, "-journal"));
            let _ = fs::remove_file(sidecar_path(&self.path, "-wal"));
            let _ = fs::remove_file(sidecar_path(&self.path, "-shm"));
        }
    }
}

fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

#[cfg(test)]
mod publication_tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn publication_failure_preserves_blobs_when_database_target_exists() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("gnosis.sqlite");
        let blobs = directory.path().join("blobs");
        fs::create_dir(&blobs).unwrap();
        let mut publication = PublicationMarker::create(&database, &blobs).unwrap();
        fs::write(&database, b"committed").unwrap();

        preserve_publication_if_target_exists(&mut publication, &database);
        drop(publication);

        assert!(blobs.exists());
        assert!(publication_marker_path(&database).unwrap().exists());
    }
}
