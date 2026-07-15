use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    blob::{BlobError, BlobStore},
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

pub(crate) fn create_backup(
    store: &SqliteStore,
    blobs: &BlobStore,
    destination: &Path,
    created_at_us: u64,
) -> Result<BackupSummary, BackupError> {
    require_new_directory(destination)?;
    let staging = staging_path(destination)?;
    fs::create_dir(&staging)?;
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o700))?;
    let result = create_staging_backup(store, blobs, &staging, created_at_us);
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
    store: &SqliteStore,
    blobs: &BlobStore,
    staging: &Path,
    created_at_us: u64,
) -> Result<BackupSummary, BackupError> {
    let database = staging.join(DATABASE_FILE);
    store.backup_database(&database)?;
    fs::set_permissions(&database, fs::Permissions::from_mode(0o600))?;
    let rows = store.referenced_blobs()?;
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
    Ok(summary(path, &manifest))
}

pub(crate) fn validate_restore(
    backup: &Path,
    temporary: &Path,
) -> Result<BackupSummary, BackupError> {
    let summary = verify_backup(backup)?;
    require_new_directory(temporary)?;
    fs::create_dir(temporary)?;
    fs::set_permissions(temporary, fs::Permissions::from_mode(0o700))?;
    let result = materialize_restore(backup, temporary);
    let cleanup = fs::remove_dir_all(temporary);
    result?;
    cleanup?;
    Ok(summary)
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

fn materialize_restore(backup: &Path, temporary: &Path) -> Result<(), BackupError> {
    fs::copy(backup.join(DATABASE_FILE), temporary.join(DATABASE_FILE))?;
    fs::set_permissions(
        temporary.join(DATABASE_FILE),
        fs::Permissions::from_mode(0o600),
    )?;
    let manifest = read_manifest(&backup.join(MANIFEST_FILE))?;
    for blob in &manifest.blobs {
        copy_manifest_blob(backup, temporary, blob)?;
    }
    let rows = SqliteStore::verify_database_file(&temporary.join(DATABASE_FILE))?;
    if rows.len() == manifest.blobs.len() {
        Ok(())
    } else {
        Err(BackupError::BlobManifest)
    }
}

fn copy_manifest_blob(
    backup: &Path,
    temporary: &Path,
    blob: &ManifestBlob,
) -> Result<(), BackupError> {
    let relative = blob_relative_path(&blob.sha256)?;
    let target = temporary.join("blobs").join(&relative);
    fs::create_dir_all(target.parent().ok_or(BackupError::Format)?)?;
    fs::copy(backup.join("blobs").join(relative), &target)?;
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600))?;
    verify_file(&target, &blob.sha256, blob.size_bytes)?;
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
    if digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
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
    #[error("backup filesystem operation failed")]
    Io(#[from] io::Error),
    #[error("backup database operation failed")]
    Store(#[from] StoreError),
    #[error("backup blob operation failed")]
    Blob(#[from] BlobError),
}
