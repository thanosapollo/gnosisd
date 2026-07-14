//! Offline migration of a legacy Emacs Gnosis database.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

use rusqlite::{Connection, OpenFlags, backup::Backup};
use thiserror::Error;

use crate::store::initialize_legacy_snapshot;

const LEGACY_SCHEMA_VERSION: u32 = 9;

/// Summary of a completed legacy database migration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LegacyMigrationSummary {
    source_schema_version: u32,
    legacy_table_count: u64,
    legacy_row_count: u64,
}

impl LegacyMigrationSummary {
    /// Return the imported Emacs database schema version.
    pub const fn source_schema_version(self) -> u32 {
        self.source_schema_version
    }

    /// Return the number of preserved legacy tables.
    pub const fn legacy_table_count(self) -> u64 {
        self.legacy_table_count
    }

    /// Return the total number of preserved legacy rows.
    pub const fn legacy_row_count(self) -> u64 {
        self.legacy_row_count
    }
}

/// Failure while creating a daemon database from a legacy Emacs database.
#[derive(Debug, Error)]
pub enum LegacyMigrationError {
    #[error("source database is not a regular file")]
    InvalidSource,
    #[error("source and target database paths must differ")]
    SameDatabase,
    #[error("source database is currently leased by Emacs")]
    SourceLeased,
    #[error("target database already exists")]
    TargetExists,
    #[error("migration staging database already exists")]
    StagingExists,
    #[error("legacy database schema version {found} is unsupported")]
    UnsupportedSourceSchema { found: u32 },
    #[error("legacy database schema is incomplete")]
    IncompleteSourceSchema,
    #[error("source database failed integrity validation")]
    SourceIntegrity,
    #[error("migrated database failed integrity validation")]
    TargetIntegrity,
    #[error("legacy row counts changed during migration")]
    ManifestMismatch,
    #[error("migration filesystem operation failed")]
    Io(#[source] io::Error),
    #[error("migration database operation failed")]
    Database(#[source] rusqlite::Error),
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

/// Copy a schema-v9 Emacs database into a new daemon-owned database.
///
/// The source is opened read-only and is never modified. The target must not
/// exist. A complete SQLite snapshot is validated before it is published.
pub fn migrate_legacy_database(
    source: impl AsRef<Path>,
    target: impl AsRef<Path>,
) -> Result<LegacyMigrationSummary, LegacyMigrationError> {
    let paths = MigrationPaths::new(source.as_ref(), target.as_ref())?;
    paths.require_available()?;
    reject_emacs_lease(&paths.source)?;
    let source = open_source(&paths.source)?;
    validate_integrity(&source, LegacyMigrationError::SourceIntegrity)?;
    let version = source_schema_version(&source)?;
    let manifest = legacy_manifest(&source)?;
    require_legacy_schema(&source, &manifest)?;
    let mut staging = StagingDatabase::create(paths.staging)?;
    migrate_snapshot(&source, staging.path(), version, &manifest)?;
    staging.publish(&paths.target)?;
    summary(version, &manifest)
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
    let identifier = format!("\"{}\"", name.replace('"', "\"\""));
    let count: i64 =
        connection.query_row(&format!("SELECT count(*) FROM {identifier}"), [], |row| {
            row.get(0)
        })?;
    let count = u64::try_from(count).map_err(|_| LegacyMigrationError::SourceIntegrity)?;
    Ok((name, count))
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

fn migrate_snapshot(
    source: &Connection,
    staging: &Path,
    version: u32,
    manifest: &BTreeMap<String, u64>,
) -> Result<(), LegacyMigrationError> {
    let mut target = Connection::open_with_flags(staging, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    Backup::new(source, &mut target)?.run_to_completion(1024, Duration::from_millis(1), None)?;
    if legacy_manifest(&target)? != *manifest {
        return Err(LegacyMigrationError::ManifestMismatch);
    }
    initialize_snapshot(&target, version, manifest)?;
    verify_migrated_manifest(&target, manifest)?;
    validate_integrity(&target, LegacyMigrationError::TargetIntegrity)
}

fn initialize_snapshot(
    connection: &Connection,
    version: u32,
    manifest: &BTreeMap<String, u64>,
) -> Result<(), LegacyMigrationError> {
    let tables =
        i64::try_from(manifest.len()).map_err(|_| LegacyMigrationError::SourceIntegrity)?;
    let rows =
        i64::try_from(total_rows(manifest)?).map_err(|_| LegacyMigrationError::SourceIntegrity)?;
    initialize_legacy_snapshot(connection, i64::from(version), tables, rows)?;
    Ok(())
}

fn verify_migrated_manifest(
    connection: &Connection,
    manifest: &BTreeMap<String, u64>,
) -> Result<(), LegacyMigrationError> {
    for (name, expected) in manifest {
        let migrated_name = if name == "node_tag" {
            "legacy_node_tag"
        } else {
            name
        };
        let (_, actual) = table_manifest_entry(connection, migrated_name.to_owned())?;
        if actual != *expected {
            return Err(LegacyMigrationError::ManifestMismatch);
        }
    }
    Ok(())
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

fn summary(
    version: u32,
    manifest: &BTreeMap<String, u64>,
) -> Result<LegacyMigrationSummary, LegacyMigrationError> {
    Ok(LegacyMigrationSummary {
        source_schema_version: version,
        legacy_table_count: u64::try_from(manifest.len())
            .map_err(|_| LegacyMigrationError::SourceIntegrity)?,
        legacy_row_count: total_rows(manifest)?,
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
        if let Err(error) = fs::remove_file(&self.path) {
            let _ = fs::remove_file(target);
            return Err(error.into());
        }
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
