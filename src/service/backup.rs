use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    blob::{BlobError, BlobStore},
    is_canonical_sha256,
    store::{SqliteStore, StoreError},
};

const DATABASE_FILE: &str = "gnosis.sqlite";
const MANIFEST_FILE: &str = "manifest.json";
const BACKUP_FORMAT_VERSION: u32 = 1;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct BackupSummary {
    pub(crate) path: PathBuf,
    pub(crate) created_at_us: u64,
    pub(crate) database_sha256: String,
    pub(crate) blob_count: u64,
}

pub(crate) struct BackupSnapshot {
    database: PathBuf,
    blobs: Vec<(String, u64)>,
}

impl Drop for BackupSnapshot {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.database);
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct BackupManifest {
    format_version: u32,
    created_at_us: u64,
    database_sha256: String,
    blobs: Vec<ManifestBlob>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
struct ManifestBlob {
    sha256: String,
    size_bytes: u64,
}

pub(crate) fn snapshot_backup(
    source: &Path,
    destination: &Path,
) -> Result<BackupSnapshot, BackupError> {
    require_new_directory(destination)?;
    let parent = destination.parent().ok_or(BackupError::Destination)?;
    let database = parent.join(format!(".gnosis-backup-snapshot-{}.sqlite", Uuid::now_v7()));
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&database)?;
    if let Err(error) = SqliteStore::backup_database(source, &database) {
        let _ = fs::remove_file(&database);
        return Err(error.into());
    }
    let blobs = match SqliteStore::verify_database_file(&database) {
        Ok(blobs) => blobs,
        Err(error) => {
            let _ = fs::remove_file(&database);
            return Err(error.into());
        }
    };
    Ok(BackupSnapshot { database, blobs })
}

pub(crate) fn create_backup(
    snapshot: BackupSnapshot,
    blobs: &BlobStore,
    destination: &Path,
    created_at_us: u64,
) -> Result<BackupSummary, BackupError> {
    let result = create_backup_inner(&snapshot, blobs, destination, created_at_us);
    let cleanup = fs::remove_file(&snapshot.database);
    result.and_then(|summary| {
        cleanup?;
        Ok(summary)
    })
}

fn create_backup_inner(
    snapshot: &BackupSnapshot,
    blobs: &BlobStore,
    destination: &Path,
    created_at_us: u64,
) -> Result<BackupSummary, BackupError> {
    require_new_directory(destination)?;
    let staging = staging_path(destination)?;
    fs::create_dir(&staging)?;
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o700))?;
    let result = create_staging_backup(snapshot, blobs, &staging, created_at_us);
    let summary = match result {
        Ok(summary) => summary,
        Err(error) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
    };
    if let Err(error) = verify_backup(&staging) {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    File::open(&staging)?.sync_all()?;
    fs::rename(&staging, destination)?;
    sync_parent(destination)?;
    Ok(BackupSummary {
        path: destination.to_owned(),
        ..summary
    })
}

fn create_staging_backup(
    snapshot: &BackupSnapshot,
    blobs: &BlobStore,
    staging: &Path,
    created_at_us: u64,
) -> Result<BackupSummary, BackupError> {
    let database = staging.join(DATABASE_FILE);
    fs::copy(&snapshot.database, &database)?;
    fs::set_permissions(&database, fs::Permissions::from_mode(0o600))?;
    let rows = snapshot.blobs.clone();
    for (digest, size) in &rows {
        blobs.copy_to(digest, *size, &staging.join("blobs"))?;
    }
    let manifest = BackupManifest {
        format_version: BACKUP_FORMAT_VERSION,
        created_at_us,
        database_sha256: sha256_file(&database)?,
        blobs: rows
            .into_iter()
            .map(|(sha256, size_bytes)| ManifestBlob { sha256, size_bytes })
            .collect(),
    };
    write_manifest(&staging.join(MANIFEST_FILE), &manifest)?;
    Ok(summary(staging, &manifest))
}

pub(crate) fn verify_backup(path: &Path) -> Result<BackupSummary, BackupError> {
    let manifest = verified_manifest(path)?;
    Ok(summary(path, &manifest))
}

fn verified_manifest(path: &Path) -> Result<BackupManifest, BackupError> {
    require_directory(path)?;
    let manifest = read_manifest(&path.join(MANIFEST_FILE))?;
    if manifest.format_version != BACKUP_FORMAT_VERSION {
        return Err(BackupError::Format);
    }
    let database = path.join(DATABASE_FILE);
    if sha256_file(&database)? != manifest.database_sha256 {
        return Err(BackupError::DatabaseDigest);
    }
    let rows = SqliteStore::verify_database_file(&database)?;
    let expected = manifest
        .blobs
        .iter()
        .map(|blob| (blob.sha256.clone(), blob.size_bytes))
        .collect::<Vec<_>>();
    if rows != expected {
        return Err(BackupError::BlobManifest);
    }
    verify_manifest_blobs(path, &manifest.blobs)?;
    Ok(manifest)
}

pub(crate) fn validate_restore(
    backup: &Path,
    temporary: &Path,
) -> Result<BackupSummary, BackupError> {
    let manifest = verified_manifest(backup)?;
    let summary = summary(backup, &manifest);
    require_new_directory(temporary)?;
    fs::create_dir(temporary)?;
    fs::set_permissions(temporary, fs::Permissions::from_mode(0o700))?;
    let result = materialize_restore(backup, temporary, &manifest);
    let cleanup = fs::remove_dir_all(temporary);
    result?;
    cleanup?;
    Ok(summary)
}

pub(crate) fn restore_backup(
    backup: &Path,
    database: &Path,
    blob_root: &Path,
) -> Result<PathBuf, BackupError> {
    restore_backup_with_hook(backup, database, blob_root, |_| Ok(()))
}

fn restore_backup_with_hook<F>(
    backup: &Path,
    database: &Path,
    blob_root: &Path,
    before_activation: F,
) -> Result<PathBuf, BackupError>
where
    F: FnOnce(&Path) -> Result<(), BackupError>,
{
    let staging = restore_staging_path(database, blob_root)?;
    let rollback = restore_rollback_path(database, blob_root)?;
    if rollback.exists() {
        return Err(BackupError::RollbackExists);
    }
    let manifest = verified_manifest(backup)?;
    let backup = fs::canonicalize(backup)?;
    if backup.starts_with(&staging) || staging.starts_with(&backup) {
        return Err(BackupError::Destination);
    }
    if staging.exists() {
        require_directory(&staging)?;
        fs::remove_dir_all(&staging)?;
        sync_parent(&staging)?;
    }
    fs::create_dir(&staging)?;
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o700))?;
    if let Err(error) = materialize_restore(&backup, &staging, &manifest) {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    File::open(&staging)?.sync_all()?;
    before_activation(&staging)?;
    activate_restore(&staging, &rollback, database, blob_root)?;
    Ok(rollback)
}

pub(crate) fn prune_backups(directory: &Path, keep: u16) -> Result<Vec<PathBuf>, BackupError> {
    require_directory(directory)?;
    if keep == 0 {
        return Err(BackupError::Destination);
    }
    let mut backups = fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    backups.retain(|path| is_managed_backup(path));
    backups.sort();
    let remove = backups.len().saturating_sub(usize::from(keep));
    let removed = backups.into_iter().take(remove).collect::<Vec<_>>();
    for backup in &removed {
        require_directory(backup)?;
        fs::remove_dir_all(backup)?;
    }
    File::open(directory)?.sync_all()?;
    Ok(removed)
}

fn is_managed_backup(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let bytes = name.as_bytes();
    bytes.len() == 23
        && bytes.starts_with(b"gnosis-")
        && bytes[7..15].iter().all(u8::is_ascii_digit)
        && bytes[15] == b'T'
        && bytes[16..22].iter().all(u8::is_ascii_digit)
        && bytes[22] == b'Z'
}

fn materialize_restore(
    backup: &Path,
    temporary: &Path,
    manifest: &BackupManifest,
) -> Result<(), BackupError> {
    fs::copy(backup.join(DATABASE_FILE), temporary.join(DATABASE_FILE))?;
    fs::set_permissions(
        temporary.join(DATABASE_FILE),
        fs::Permissions::from_mode(0o600),
    )?;
    File::open(temporary.join(DATABASE_FILE))?.sync_all()?;
    let restored_blobs = temporary.join("blobs");
    fs::create_dir(&restored_blobs)?;
    fs::set_permissions(&restored_blobs, fs::Permissions::from_mode(0o700))?;
    for blob in &manifest.blobs {
        copy_manifest_blob(backup, temporary, blob)?;
    }
    File::open(&restored_blobs)?.sync_all()?;
    let database = temporary.join(DATABASE_FILE);
    if sha256_file(&database)? != manifest.database_sha256 {
        return Err(BackupError::DatabaseDigest);
    }
    let rows = SqliteStore::verify_database_file(&database)?;
    let expected = manifest
        .blobs
        .iter()
        .map(|blob| (blob.sha256.clone(), blob.size_bytes))
        .collect::<Vec<_>>();
    if rows != expected {
        return Err(BackupError::BlobManifest);
    }
    verify_manifest_blobs(temporary, &manifest.blobs)?;
    for sidecar in sqlite_sidecar_paths(&database) {
        match fs::symlink_metadata(sidecar) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => return Err(BackupError::Format),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn activate_restore(
    staging: &Path,
    rollback: &Path,
    database: &Path,
    blob_root: &Path,
) -> Result<(), BackupError> {
    if database.exists() {
        require_regular_file(database)?;
    }
    if blob_root.exists() {
        require_directory(blob_root)?;
    }
    let sidecars = sqlite_sidecar_paths(database);
    for sidecar in &sidecars {
        match fs::symlink_metadata(sidecar) {
            Ok(metadata) if metadata.file_type().is_file() => {}
            Ok(_) => return Err(BackupError::Format),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    fs::create_dir(rollback)?;
    fs::set_permissions(rollback, fs::Permissions::from_mode(0o700))?;
    let rollback_database = rollback.join(DATABASE_FILE);
    let rollback_blobs = rollback.join("blobs");
    let relocation = (|| {
        if database.exists() {
            fs::rename(database, &rollback_database)?;
        }
        if blob_root.exists() {
            fs::rename(blob_root, &rollback_blobs)?;
        }
        for sidecar in &sidecars {
            if sidecar.exists() {
                fs::rename(
                    sidecar,
                    rollback.join(sidecar.file_name().ok_or(BackupError::Destination)?),
                )?;
            }
        }
        File::open(rollback)?.sync_all()?;
        sync_parent(database)
    })();
    if let Err(error) = relocation {
        rollback_failed_activation(rollback, database, blob_root, &sidecars, false)?;
        return Err(error);
    }

    let activation = (|| {
        fs::rename(staging.join(DATABASE_FILE), database)?;
        fs::rename(staging.join("blobs"), blob_root)?;
        fs::remove_dir(staging)?;
        sync_parent(database)
    })();
    if let Err(error) = activation {
        rollback_failed_activation(rollback, database, blob_root, &sidecars, true)?;
        return Err(error);
    }
    Ok(())
}

fn rollback_failed_activation(
    rollback: &Path,
    database: &Path,
    blob_root: &Path,
    sidecars: &[PathBuf; 3],
    remove_activated_targets: bool,
) -> Result<(), BackupError> {
    if remove_activated_targets && database.exists() {
        require_regular_file(database)?;
        fs::remove_file(database)?;
    }
    if remove_activated_targets && blob_root.exists() {
        require_directory(blob_root)?;
        fs::remove_dir_all(blob_root)?;
    }
    let rollback_database = rollback.join(DATABASE_FILE);
    let rollback_blobs = rollback.join("blobs");
    if rollback_database.exists() {
        fs::rename(rollback_database, database)?;
    }
    if rollback_blobs.exists() {
        fs::rename(rollback_blobs, blob_root)?;
    }
    for sidecar in sidecars {
        let rollback_sidecar = rollback.join(sidecar.file_name().ok_or(BackupError::Destination)?);
        if rollback_sidecar.exists() {
            if remove_activated_targets && sidecar.exists() {
                fs::remove_file(sidecar)?;
            }
            fs::rename(rollback_sidecar, sidecar)?;
        }
    }
    fs::remove_dir(rollback)?;
    sync_parent(database)
}

fn sqlite_sidecar_paths(database: &Path) -> [PathBuf; 3] {
    ["-journal", "-wal", "-shm"].map(|suffix| {
        let mut path = database.as_os_str().to_os_string();
        path.push(suffix);
        PathBuf::from(path)
    })
}

fn copy_manifest_blob(
    backup: &Path,
    temporary: &Path,
    blob: &ManifestBlob,
) -> Result<(), BackupError> {
    let relative = blob_relative_path(&blob.sha256)?;
    let target = temporary.join("blobs").join(&relative);
    let parent = target.parent().ok_or(BackupError::Format)?;
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    fs::copy(backup.join("blobs").join(relative), &target)?;
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600))?;
    verify_file(&target, &blob.sha256, blob.size_bytes)?;
    File::open(&target)?.sync_all()?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn verify_manifest_blobs(path: &Path, blobs: &[ManifestBlob]) -> Result<(), BackupError> {
    let expected = blobs
        .iter()
        .map(|blob| blob_relative_path(&blob.sha256))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let actual = backup_blob_paths(&path.join("blobs"))?;
    if actual != expected {
        return Err(BackupError::BlobManifest);
    }
    for blob in blobs {
        let file = path.join("blobs").join(blob_relative_path(&blob.sha256)?);
        verify_file(&file, &blob.sha256, blob.size_bytes)?;
    }
    Ok(())
}

fn backup_blob_paths(root: &Path) -> Result<BTreeSet<PathBuf>, BackupError> {
    if !root.exists() {
        return Ok(BTreeSet::new());
    }
    require_directory(root)?;
    let mut paths = BTreeSet::new();
    for directory in fs::read_dir(root)? {
        let directory = directory?;
        require_directory(&directory.path())?;
        for file in fs::read_dir(directory.path())? {
            let file = file?;
            require_regular_file(&file.path())?;
            paths.insert(PathBuf::from(directory.file_name()).join(file.file_name()));
        }
    }
    Ok(paths)
}

fn write_manifest(path: &Path, manifest: &BackupManifest) -> Result<(), BackupError> {
    let bytes = serde_json::to_vec_pretty(manifest).map_err(|_| BackupError::Format)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}

fn read_manifest(path: &Path) -> Result<BackupManifest, BackupError> {
    require_regular_file(path)?;
    let bytes = fs::read(path)?;
    if bytes.len() > 16 * 1024 * 1024 {
        return Err(BackupError::Format);
    }
    serde_json::from_slice(&bytes).map_err(|_| BackupError::Format)
}

fn summary(path: &Path, manifest: &BackupManifest) -> BackupSummary {
    BackupSummary {
        path: path.to_owned(),
        created_at_us: manifest.created_at_us,
        database_sha256: manifest.database_sha256.clone(),
        blob_count: manifest.blobs.len() as u64,
    }
}

fn require_new_directory(path: &Path) -> Result<(), BackupError> {
    if path.exists() || path.file_name().is_none() {
        return Err(BackupError::Destination);
    }
    require_directory(path.parent().ok_or(BackupError::Destination)?)
}

fn staging_path(destination: &Path) -> Result<PathBuf, BackupError> {
    let name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(BackupError::Destination)?;
    Ok(destination
        .parent()
        .ok_or(BackupError::Destination)?
        .join(format!(".{name}.staging-{}", Uuid::now_v7())))
}

fn restore_staging_path(database: &Path, blob_root: &Path) -> Result<PathBuf, BackupError> {
    restore_bundle_path(database, blob_root, "staging")
}

fn restore_rollback_path(database: &Path, blob_root: &Path) -> Result<PathBuf, BackupError> {
    restore_bundle_path(database, blob_root, "rollback")
}

fn restore_bundle_path(
    database: &Path,
    blob_root: &Path,
    suffix: &str,
) -> Result<PathBuf, BackupError> {
    let parent = database.parent().ok_or(BackupError::Destination)?;
    if blob_root.parent() != Some(parent) {
        return Err(BackupError::Destination);
    }
    require_directory(parent)?;
    let database_name = database
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(BackupError::Destination)?;
    if blob_root.file_name().is_none() {
        return Err(BackupError::Destination);
    }
    Ok(parent.join(format!(".{database_name}.restore-{suffix}")))
}

fn require_directory(path: &Path) -> Result<(), BackupError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_dir() {
        Ok(())
    } else {
        Err(BackupError::Format)
    }
}

fn require_regular_file(path: &Path) -> Result<(), BackupError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_file() {
        Ok(())
    } else {
        Err(BackupError::Format)
    }
}

fn blob_relative_path(digest: &str) -> Result<PathBuf, BackupError> {
    if is_canonical_sha256(digest) {
        Ok(PathBuf::from(&digest[..2]).join(digest))
    } else {
        Err(BackupError::Format)
    }
}

fn verify_file(path: &Path, digest: &str, size: u64) -> Result<(), BackupError> {
    require_regular_file(path)?;
    let metadata = fs::metadata(path)?;
    if metadata.len() != size || sha256_file(path)? != digest {
        Err(BackupError::BlobDigest)
    } else {
        Ok(())
    }
}

fn sha256_file(path: &Path) -> Result<String, BackupError> {
    require_regular_file(path)?;
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

fn sync_parent(path: &Path) -> Result<(), BackupError> {
    File::open(path.parent().ok_or(BackupError::Destination)?)?.sync_all()?;
    Ok(())
}

#[derive(Debug, Error)]
pub(crate) enum BackupError {
    #[error("backup destination is invalid or already exists")]
    Destination,
    #[error("backup format is invalid")]
    Format,
    #[error("backup database digest does not match")]
    DatabaseDigest,
    #[error("backup blob manifest does not match its database")]
    BlobManifest,
    #[error("backup blob content does not match its digest")]
    BlobDigest,
    #[error("a previous restore rollback must be resolved first")]
    RollbackExists,
    #[cfg(test)]
    #[error("restore interrupted before activation")]
    Interrupted,
    #[error("backup filesystem operation failed")]
    Io(#[from] io::Error),
    #[error("backup database operation failed")]
    Store(#[from] StoreError),
    #[error("backup blob operation failed")]
    Blob(#[from] BlobError),
}

#[cfg(test)]
mod tests {
    use std::{fs, mem, os::unix::fs::PermissionsExt as _, path::Path};

    use rusqlite::Connection;
    use tempfile::tempdir;

    use super::{
        BackupError, create_backup, read_manifest, restore_backup, restore_backup_with_hook,
        restore_staging_path, sha256_file, snapshot_backup, sqlite_sidecar_paths,
    };
    use crate::{
        application::{canonical_asset_path, prepare_asset},
        blob::BlobStore,
        store::{SqliteStore, StoreError},
    };

    fn backup_fixture(root: &Path) -> (std::path::PathBuf, String, Vec<u8>) {
        let source_database = root.join("source.sqlite");
        let source_blobs = root.join("source-blobs");
        let blobs = BlobStore::open(&source_blobs).unwrap();
        let bytes = b"cold restore asset".to_vec();
        let blob = blobs.put(&bytes, "image/png").unwrap();
        let logical_path = canonical_asset_path(&blob.sha256, "image.png").unwrap();
        let mut store = SqliteStore::open(&source_database).unwrap();
        store
            .put_asset(
                &prepare_asset(logical_path, "image.png".into(), None).unwrap(),
                &blob,
                1,
                None,
            )
            .unwrap();
        drop(store);
        let backup = root.join("backup");
        let snapshot = snapshot_backup(&source_database, &backup).unwrap();
        create_backup(snapshot, &blobs, &backup, 2).unwrap();
        (backup, blob.sha256, bytes)
    }

    fn rewrite_database_digest(backup: &Path) {
        let mut manifest = read_manifest(&backup.join("manifest.json")).unwrap();
        manifest.database_sha256 = sha256_file(&backup.join("gnosis.sqlite")).unwrap();
        fs::write(
            backup.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn restore_keeps_old_target_until_verified_and_preserves_rollback() {
        let directory = tempdir().unwrap();
        let (backup, digest, bytes) = backup_fixture(directory.path());
        let database = directory.path().join("gnosis.sqlite");
        let blob_root = directory.path().join("blobs");
        drop(SqliteStore::open(&database).unwrap());
        let old_database = fs::read(&database).unwrap();
        let old_blobs = BlobStore::open(&blob_root).unwrap();
        let old_blob = old_blobs.put(b"old target", "text/plain").unwrap();

        let interrupted = restore_backup_with_hook(&backup, &database, &blob_root, |_| {
            Err(BackupError::Interrupted)
        });
        assert!(matches!(interrupted, Err(BackupError::Interrupted)));
        assert_eq!(fs::read(&database).unwrap(), old_database);
        assert_eq!(old_blobs.read(&old_blob.sha256).unwrap(), b"old target");
        assert!(
            restore_staging_path(&database, &blob_root)
                .unwrap()
                .exists()
        );

        let activation_failed =
            restore_backup_with_hook(&backup, &database, &blob_root, |staging| {
                fs::remove_dir_all(staging.join("blobs")).unwrap();
                Ok(())
            });
        assert!(matches!(activation_failed, Err(BackupError::Io(_))));
        assert_eq!(fs::read(&database).unwrap(), old_database);
        assert_eq!(old_blobs.read(&old_blob.sha256).unwrap(), b"old target");
        assert!(
            !super::restore_rollback_path(&database, &blob_root)
                .unwrap()
                .exists()
        );

        let rollback = restore_backup(&backup, &database, &blob_root).unwrap();
        assert_eq!(
            fs::read(rollback.join("gnosis.sqlite")).unwrap(),
            old_database
        );
        assert_eq!(
            BlobStore::open(rollback.join("blobs"))
                .unwrap()
                .read(&old_blob.sha256)
                .unwrap(),
            b"old target"
        );
        assert_eq!(
            BlobStore::open(&blob_root).unwrap().read(&digest).unwrap(),
            bytes
        );
        assert_eq!(
            fs::metadata(&database).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&blob_root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(
            !restore_staging_path(&database, &blob_root)
                .unwrap()
                .exists()
        );
        assert!(matches!(
            restore_backup(&backup, &database, &blob_root),
            Err(BackupError::RollbackExists)
        ));
        assert_eq!(
            fs::read(rollback.join("gnosis.sqlite")).unwrap(),
            old_database
        );
    }

    #[test]
    fn restore_quarantines_hot_sqlite_sidecars() {
        let directory = tempdir().unwrap();
        let (backup, _, _) = backup_fixture(directory.path());
        let database = directory.path().join("gnosis.sqlite");
        let blob_root = directory.path().join("blobs");
        fs::copy(backup.join("gnosis.sqlite"), &database).unwrap();
        fs::create_dir(&blob_root).unwrap();

        let backup_timezone: String = Connection::open(backup.join("gnosis.sqlite"))
            .unwrap()
            .query_row("SELECT timezone FROM settings WHERE id = 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        let target = Connection::open(&database).unwrap();
        target
            .execute(
                "UPDATE settings SET timezone = 'Europe/Athens' WHERE id = 1",
                [],
            )
            .unwrap();
        drop(target);
        let hot = Connection::open(&database).unwrap();
        hot.execute_batch(
            "PRAGMA journal_mode = DELETE;
             BEGIN IMMEDIATE;
             UPDATE settings SET timezone = 'Europe/Berlin' WHERE id = 1;",
        )
        .unwrap();
        let sidecars = sqlite_sidecar_paths(&database);
        assert!(sidecars[0].exists());
        fs::write(&sidecars[1], b"old wal").unwrap();
        fs::write(&sidecars[2], b"old shm").unwrap();

        let rollback = restore_backup(&backup, &database, &blob_root).unwrap();
        mem::forget(hot);

        let restored_timezone: String = Connection::open(&database)
            .unwrap()
            .query_row("SELECT timezone FROM settings WHERE id = 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(restored_timezone, backup_timezone);
        for sidecar in &sidecars {
            assert!(!sidecar.exists());
            assert!(rollback.join(sidecar.file_name().unwrap()).exists());
        }
        assert_eq!(
            fs::read(rollback.join(sidecars[1].file_name().unwrap())).unwrap(),
            b"old wal"
        );
        assert_eq!(
            fs::read(rollback.join(sidecars[2].file_name().unwrap())).unwrap(),
            b"old shm"
        );
    }

    #[test]
    fn restore_rejects_backups_at_or_beneath_staging_without_mutation() {
        for nested in [false, true] {
            let directory = tempdir().unwrap();
            let (source, _, _) = backup_fixture(directory.path());
            let database = directory.path().join("target.sqlite");
            let blob_root = directory.path().join("target-blobs");
            fs::write(&database, b"old database").unwrap();
            fs::create_dir(&blob_root).unwrap();
            fs::write(blob_root.join("old-blob"), b"old blob").unwrap();
            let sidecars = sqlite_sidecar_paths(&database);
            for sidecar in &sidecars {
                fs::write(sidecar, b"old sidecar").unwrap();
            }
            let staging = restore_staging_path(&database, &blob_root).unwrap();
            let backup = if nested {
                fs::create_dir(&staging).unwrap();
                staging.join("nested-backup")
            } else {
                staging.clone()
            };
            fs::rename(source, &backup).unwrap();

            assert!(matches!(
                restore_backup(&backup, &database, &blob_root),
                Err(BackupError::Destination)
            ));
            assert!(backup.join("manifest.json").is_file());
            assert!(backup.join("gnosis.sqlite").is_file());
            assert_eq!(fs::read(&database).unwrap(), b"old database");
            assert_eq!(fs::read(blob_root.join("old-blob")).unwrap(), b"old blob");
            for sidecar in &sidecars {
                assert_eq!(fs::read(sidecar).unwrap(), b"old sidecar");
            }
        }
    }

    #[test]
    fn restore_rejects_corrupt_or_incomplete_backups_before_target_mutation() {
        for corruption in [
            "database-hash",
            "identity",
            "schema",
            "integrity",
            "foreign-key",
            "blob",
            "extra-blob",
        ] {
            let directory = tempdir().unwrap();
            let (backup, digest, _) = backup_fixture(directory.path());
            match corruption {
                "database-hash" => fs::write(backup.join("gnosis.sqlite"), b"corrupt").unwrap(),
                "identity" => {
                    let database = Connection::open(backup.join("gnosis.sqlite")).unwrap();
                    database
                        .execute_batch("PRAGMA application_id = 0;")
                        .unwrap();
                    drop(database);
                    rewrite_database_digest(&backup);
                }
                "schema" => {
                    let database = Connection::open(backup.join("gnosis.sqlite")).unwrap();
                    database.pragma_update(None, "user_version", 999).unwrap();
                    drop(database);
                    rewrite_database_digest(&backup);
                }
                "integrity" => {
                    let database = Connection::open(backup.join("gnosis.sqlite")).unwrap();
                    database
                        .execute_batch(
                            "PRAGMA writable_schema = ON;
                             UPDATE sqlite_schema SET rootpage =
                                 (SELECT rootpage FROM sqlite_schema WHERE name = 'settings')
                             WHERE type = 'table' AND name = 'review_event';
                             PRAGMA writable_schema = OFF;",
                        )
                        .unwrap();
                    drop(database);
                    rewrite_database_digest(&backup);
                }
                "foreign-key" => {
                    let database = Connection::open(backup.join("gnosis.sqlite")).unwrap();
                    database.pragma_update(None, "foreign_keys", false).unwrap();
                    database
                        .execute(
                            "INSERT INTO node_tag(document_id, node_id, ordinal, tag) \
                             VALUES ('missing', 'missing', 0, 'bad')",
                            [],
                        )
                        .unwrap();
                    drop(database);
                    rewrite_database_digest(&backup);
                }
                "blob" => fs::write(
                    backup.join("blobs").join(&digest[..2]).join(&digest),
                    b"corrupt",
                )
                .unwrap(),
                "extra-blob" => {
                    let extra = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
                    fs::create_dir_all(backup.join("blobs").join(&extra[..2])).unwrap();
                    fs::write(backup.join("blobs").join(&extra[..2]).join(extra), b"hello")
                        .unwrap();
                }
                _ => unreachable!(),
            }
            let database = directory.path().join("target.sqlite");
            let blob_root = directory.path().join("target-blobs");
            fs::write(&database, b"old database").unwrap();
            fs::create_dir(&blob_root).unwrap();
            fs::write(blob_root.join("old-blob"), b"old blob").unwrap();
            let result = restore_backup(&backup, &database, &blob_root);
            match corruption {
                "database-hash" => {
                    assert!(matches!(result, Err(BackupError::DatabaseDigest)))
                }
                "identity" => assert!(matches!(
                    result,
                    Err(BackupError::Store(
                        StoreError::UnsupportedApplicationId { .. }
                    ))
                )),
                "schema" => assert!(matches!(
                    result,
                    Err(BackupError::Store(
                        StoreError::UnsupportedSchemaVersion { .. }
                    ))
                )),
                "integrity" => assert!(matches!(
                    result,
                    Err(BackupError::Store(StoreError::CorruptData { .. }))
                )),
                "foreign-key" => assert!(matches!(
                    result,
                    Err(BackupError::Store(StoreError::CorruptData { .. }))
                )),
                "blob" => assert!(matches!(result, Err(BackupError::BlobDigest))),
                "extra-blob" => assert!(matches!(result, Err(BackupError::BlobManifest))),
                _ => unreachable!(),
            }
            assert_eq!(fs::read(&database).unwrap(), b"old database");
            assert_eq!(fs::read(blob_root.join("old-blob")).unwrap(), b"old blob");
        }
    }
}
