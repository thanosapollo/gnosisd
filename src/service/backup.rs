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
const RESTORE_STATE_FILE: &str = "restore-state.json";
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct RestoreState {
    format_version: u32,
    complete: bool,
    had_database: bool,
    had_blobs: bool,
    had_sidecars: [bool; 3],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActivationStep {
    DatabaseRelocated,
    BlobsRelocated,
    SidecarRelocated(usize),
    DatabaseActivated,
    BlobsActivated,
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
    restore_backup_with_hooks(backup, database, blob_root, |_| Ok(()), |_| {})
}

#[cfg(test)]
fn restore_backup_with_hook<F>(
    backup: &Path,
    database: &Path,
    blob_root: &Path,
    before_activation: F,
) -> Result<PathBuf, BackupError>
where
    F: FnOnce(&Path) -> Result<(), BackupError>,
{
    restore_backup_with_hooks(backup, database, blob_root, before_activation, |_| {})
}

fn restore_backup_with_hooks<F, G>(
    backup: &Path,
    database: &Path,
    blob_root: &Path,
    before_activation: F,
    after_step: G,
) -> Result<PathBuf, BackupError>
where
    F: FnOnce(&Path) -> Result<(), BackupError>,
    G: FnMut(ActivationStep),
{
    restore_backup_with_sync_hooks(
        backup,
        database,
        blob_root,
        before_activation,
        after_step,
        sync_directory,
    )
}

fn restore_backup_with_sync_hooks<F, G, S>(
    backup: &Path,
    database: &Path,
    blob_root: &Path,
    before_activation: F,
    after_step: G,
    sync_rollback: S,
) -> Result<PathBuf, BackupError>
where
    F: FnOnce(&Path) -> Result<(), BackupError>,
    G: FnMut(ActivationStep),
    S: FnMut(&Path) -> Result<(), BackupError>,
{
    let database = canonical_restore_target(database)?;
    let blob_root = canonical_restore_target(blob_root)?;
    let staging = restore_staging_path(&database, &blob_root)?;
    let rollback = restore_rollback_path(&database, &blob_root)?;
    validate_restore_targets(&database, &blob_root)?;
    if rollback.exists() {
        let state = read_restore_state(&rollback)?;
        if state.complete {
            return Err(BackupError::RollbackExists);
        }
        recover_interrupted_activation(&staging, &rollback, &database, &blob_root, &state)?;
    }
    let backup = fs::canonicalize(backup)?;
    if [&database, &blob_root, &staging, &rollback]
        .iter()
        .any(|path| paths_overlap(&backup, path))
    {
        return Err(BackupError::Destination);
    }
    let manifest = verified_manifest(&backup)?;
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
    activate_restore(
        &staging,
        &rollback,
        &database,
        &blob_root,
        after_step,
        sync_rollback,
    )?;
    Ok(rollback)
}

pub(crate) fn ensure_restore_complete(
    database: &Path,
    blob_root: &Path,
) -> Result<(), BackupError> {
    let database = canonical_restore_target(database)?;
    let blob_root = canonical_restore_target(blob_root)?;
    let rollback = restore_rollback_path(&database, &blob_root)?;
    if !rollback.exists() {
        return Ok(());
    }
    if read_restore_state(&rollback)?.complete {
        Ok(())
    } else {
        Err(BackupError::RestoreIncomplete)
    }
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

fn activate_restore<F, S>(
    staging: &Path,
    rollback: &Path,
    database: &Path,
    blob_root: &Path,
    mut after_step: F,
    mut sync_rollback: S,
) -> Result<(), BackupError>
where
    F: FnMut(ActivationStep),
    S: FnMut(&Path) -> Result<(), BackupError>,
{
    validate_restore_targets(database, blob_root)?;
    let sidecars = sqlite_sidecar_paths(database);
    let mut state = RestoreState {
        format_version: BACKUP_FORMAT_VERSION,
        complete: false,
        had_database: database.exists(),
        had_blobs: blob_root.exists(),
        had_sidecars: sidecars.each_ref().map(|path| path.exists()),
    };
    fs::create_dir(rollback)?;
    fs::set_permissions(rollback, fs::Permissions::from_mode(0o700))?;
    write_restore_state(rollback, &state)?;
    sync_parent(rollback)?;
    let rollback_database = rollback.join(DATABASE_FILE);
    let rollback_blobs = rollback.join("blobs");
    let relocation = (|| {
        if state.had_database {
            fs::rename(database, &rollback_database)?;
            sync_parent(database)?;
            sync_rollback(rollback)?;
            after_step(ActivationStep::DatabaseRelocated);
        }
        if state.had_blobs {
            fs::rename(blob_root, &rollback_blobs)?;
            sync_parent(blob_root)?;
            sync_rollback(rollback)?;
            after_step(ActivationStep::BlobsRelocated);
        }
        for (index, sidecar) in sidecars.iter().enumerate() {
            if state.had_sidecars[index] {
                fs::rename(
                    sidecar,
                    rollback.join(sidecar.file_name().ok_or(BackupError::Destination)?),
                )?;
                sync_parent(sidecar)?;
                sync_rollback(rollback)?;
                after_step(ActivationStep::SidecarRelocated(index));
            }
        }
        Ok(())
    })();
    if let Err(error) = relocation {
        recover_interrupted_activation(staging, rollback, database, blob_root, &state)?;
        return Err(error);
    }

    let activation = (|| {
        fs::rename(staging.join(DATABASE_FILE), database)?;
        sync_parent(database)?;
        after_step(ActivationStep::DatabaseActivated);
        fs::rename(staging.join("blobs"), blob_root)?;
        sync_parent(blob_root)?;
        after_step(ActivationStep::BlobsActivated);
        fs::remove_dir(staging)?;
        sync_parent(staging)?;
        let completed = RestoreState {
            complete: true,
            ..state.clone()
        };
        write_restore_state(rollback, &completed)?;
        state.complete = true;
        Ok(())
    })();
    if let Err(error) = activation {
        recover_interrupted_activation(staging, rollback, database, blob_root, &state)?;
        return Err(error);
    }
    Ok(())
}

fn recover_interrupted_activation(
    staging: &Path,
    rollback: &Path,
    database: &Path,
    blob_root: &Path,
    state: &RestoreState,
) -> Result<(), BackupError> {
    if state.format_version != BACKUP_FORMAT_VERSION || state.complete {
        return Err(BackupError::Format);
    }
    let sidecars = sqlite_sidecar_paths(database);
    let rollback_database = rollback.join(DATABASE_FILE);
    let rollback_blobs = rollback.join("blobs");
    recover_target(database, &rollback_database, state.had_database, false)?;
    recover_target(blob_root, &rollback_blobs, state.had_blobs, true)?;
    for (index, sidecar) in sidecars.iter().enumerate() {
        let rollback_sidecar = rollback.join(sidecar.file_name().ok_or(BackupError::Destination)?);
        recover_target(sidecar, &rollback_sidecar, state.had_sidecars[index], false)?;
    }
    if staging.exists() {
        require_directory(staging)?;
        fs::remove_dir_all(staging)?;
    }
    let temporary_state = rollback.join(format!(".{RESTORE_STATE_FILE}.new"));
    if temporary_state.exists() {
        require_regular_file(&temporary_state)?;
        fs::remove_file(temporary_state)?;
    }
    fs::remove_file(rollback.join(RESTORE_STATE_FILE))?;
    fs::remove_dir(rollback)?;
    sync_parent(database)
}

fn recover_target(
    target: &Path,
    saved: &Path,
    existed: bool,
    directory: bool,
) -> Result<(), BackupError> {
    if saved.exists() {
        validate_optional_target(saved, directory)?;
        if target.exists() {
            validate_optional_target(target, directory)?;
            if directory {
                fs::remove_dir_all(target)?;
            } else {
                fs::remove_file(target)?;
            }
        }
        fs::rename(saved, target)?;
        sync_parent(target)?;
        return Ok(());
    }
    if existed {
        if !target.exists() {
            return Err(BackupError::RestoreIncomplete);
        }
        validate_optional_target(target, directory)?;
    } else if target.exists() {
        validate_optional_target(target, directory)?;
        if directory {
            fs::remove_dir_all(target)?;
        } else {
            fs::remove_file(target)?;
        }
        sync_parent(target)?;
    }
    Ok(())
}

fn write_restore_state(rollback: &Path, state: &RestoreState) -> Result<(), BackupError> {
    let bytes = serde_json::to_vec(state).map_err(|_| BackupError::Format)?;
    let path = rollback.join(RESTORE_STATE_FILE);
    let temporary = rollback.join(format!(".{RESTORE_STATE_FILE}.new"));
    if temporary.exists() {
        require_regular_file(&temporary)?;
        fs::remove_file(&temporary)?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(temporary, path)?;
    File::open(rollback)?.sync_all()?;
    Ok(())
}

fn read_restore_state(rollback: &Path) -> Result<RestoreState, BackupError> {
    require_directory(rollback)?;
    let path = rollback.join(RESTORE_STATE_FILE);
    require_regular_file(&path)?;
    if fs::symlink_metadata(&path)?.len() > 4096 {
        return Err(BackupError::Format);
    }
    let bytes = fs::read(path)?;
    let state: RestoreState = serde_json::from_slice(&bytes).map_err(|_| BackupError::Format)?;
    if state.format_version != BACKUP_FORMAT_VERSION {
        return Err(BackupError::Format);
    }
    Ok(state)
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
    let database = canonical_restore_target(database)?;
    let blob_root = canonical_restore_target(blob_root)?;
    let parent = database.parent().ok_or(BackupError::Destination)?;
    if blob_root.parent() != Some(parent) {
        return Err(BackupError::Destination);
    }
    let database_name = database
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(BackupError::Destination)?;
    let bundle = parent.join(format!(".{database_name}.restore-{suffix}"));
    if paths_overlap(&bundle, &database) || paths_overlap(&bundle, &blob_root) {
        return Err(BackupError::Destination);
    }
    Ok(bundle)
}

fn canonical_restore_target(path: &Path) -> Result<PathBuf, BackupError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    require_directory(parent)?;
    let parent = fs::canonicalize(parent)?;
    let name = path.file_name().ok_or(BackupError::Destination)?;
    Ok(parent.join(name))
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn validate_restore_targets(database: &Path, blob_root: &Path) -> Result<(), BackupError> {
    validate_optional_target(database, false)?;
    validate_optional_target(blob_root, true)?;
    for sidecar in sqlite_sidecar_paths(database) {
        match fs::symlink_metadata(&sidecar) {
            Ok(metadata) if metadata.file_type().is_file() => {}
            Ok(_) => return Err(BackupError::Format),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn validate_optional_target(path: &Path, directory: bool) -> Result<(), BackupError> {
    match fs::symlink_metadata(path) {
        Ok(metadata)
            if (directory && metadata.file_type().is_dir())
                || (!directory && metadata.file_type().is_file()) =>
        {
            Ok(())
        }
        Ok(_) => Err(BackupError::Format),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
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

fn sync_directory(path: &Path) -> Result<(), BackupError> {
    File::open(path)?.sync_all()?;
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
    #[error("cold restore activation is incomplete")]
    RestoreIncomplete,
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
    use std::{
        cell::{Cell, RefCell},
        fs, mem,
        os::unix::fs::{PermissionsExt as _, symlink},
        path::Path,
        process::Command,
    };

    use rusqlite::Connection;
    use tempfile::tempdir;

    use super::{
        ActivationStep, BackupError, canonical_restore_target, create_backup,
        ensure_restore_complete, read_manifest, restore_backup, restore_backup_with_hook,
        restore_backup_with_hooks, restore_backup_with_sync_hooks, restore_staging_path,
        sha256_file, snapshot_backup, sqlite_sidecar_paths, sync_directory,
    };
    use crate::{
        application::{canonical_asset_path, prepare_asset},
        blob::BlobStore,
        protocol::{DaemonConfig, DaemonServer, ServerError},
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

    #[tokio::test]
    async fn interrupted_activation_blocks_startup_and_next_restore_recovers_old_pair() {
        for interrupted_after in [
            ActivationStep::DatabaseRelocated,
            ActivationStep::BlobsRelocated,
            ActivationStep::SidecarRelocated(0),
            ActivationStep::SidecarRelocated(1),
            ActivationStep::SidecarRelocated(2),
            ActivationStep::DatabaseActivated,
            ActivationStep::BlobsActivated,
        ] {
            let directory = tempdir().unwrap();
            let (backup, digest, restored_blob) = backup_fixture(directory.path());
            let database = directory.path().join("gnosis.sqlite");
            let blob_root = directory.path().join("blobs");
            drop(SqliteStore::open(&database).unwrap());
            let old_database = fs::read(&database).unwrap();
            let old_blobs = BlobStore::open(&blob_root).unwrap();
            let old_blob = old_blobs.put(b"old target", "text/plain").unwrap();
            let sidecars = sqlite_sidecar_paths(&database);
            for (index, sidecar) in sidecars.iter().enumerate() {
                fs::write(sidecar, format!("old sidecar {index}")).unwrap();
            }

            let status = Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("service::backup::tests::cold_restore_interruption_child")
                .env("GNOSIS_TEST_RESTORE_BACKUP", &backup)
                .env("GNOSIS_TEST_RESTORE_DATABASE", &database)
                .env("GNOSIS_TEST_RESTORE_BLOBS", &blob_root)
                .env("GNOSIS_TEST_RESTORE_STEP", format!("{interrupted_after:?}"))
                .status()
                .unwrap();
            assert_eq!(status.code(), Some(91));
            assert!(matches!(
                ensure_restore_complete(&database, &blob_root),
                Err(BackupError::RestoreIncomplete)
            ));
            let mut config = DaemonConfig::new(directory.path().join("gnosis.sock"), &database);
            config.blob_path = blob_root.clone();
            assert!(matches!(
                DaemonServer::bind(config).await,
                Err(ServerError::Restore(_))
            ));

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
            for (index, sidecar) in sidecars.iter().enumerate() {
                assert_eq!(
                    fs::read(rollback.join(sidecar.file_name().unwrap())).unwrap(),
                    format!("old sidecar {index}").as_bytes()
                );
            }
            assert_eq!(
                BlobStore::open(&blob_root).unwrap().read(&digest).unwrap(),
                restored_blob
            );
            ensure_restore_complete(&database, &blob_root).unwrap();
        }
    }

    #[test]
    fn rollback_directory_sync_failure_prevents_relocation_step_advancement() {
        for failed_sync in 0..5 {
            let directory = tempdir().unwrap();
            let (backup, _, _) = backup_fixture(directory.path());
            let database = directory.path().join("gnosis.sqlite");
            let blob_root = directory.path().join("blobs");
            let sidecars = sqlite_sidecar_paths(&database);
            fs::write(&database, b"old database").unwrap();
            let old_database = fs::read(&database).unwrap();
            fs::create_dir(&blob_root).unwrap();
            fs::write(blob_root.join("old"), b"old blob").unwrap();
            for (index, sidecar) in sidecars.iter().enumerate() {
                fs::write(sidecar, format!("old sidecar {index}")).unwrap();
            }

            let reported = RefCell::new(Vec::new());
            let sync_count = Cell::new(0_usize);
            let error = restore_backup_with_sync_hooks(
                &backup,
                &database,
                &blob_root,
                |_| Ok(()),
                |step| reported.borrow_mut().push(step),
                |rollback| {
                    let current = sync_count.get();
                    sync_count.set(current + 1);
                    if current == failed_sync {
                        return Err(std::io::Error::other("injected rollback sync failure").into());
                    }
                    sync_directory(rollback)
                },
            )
            .unwrap_err();

            assert!(matches!(error, BackupError::Io(_)));
            assert_eq!(reported.borrow().len(), failed_sync);
            assert_eq!(fs::read(&database).unwrap(), old_database);
            assert_eq!(fs::read(blob_root.join("old")).unwrap(), b"old blob");
            for (index, sidecar) in sidecars.iter().enumerate() {
                assert_eq!(
                    fs::read(sidecar).unwrap(),
                    format!("old sidecar {index}").as_bytes()
                );
            }
            assert!(
                !restore_staging_path(&database, &blob_root)
                    .unwrap()
                    .exists()
            );
            ensure_restore_complete(&database, &blob_root).unwrap();
        }
    }

    #[test]
    fn cold_restore_interruption_child() {
        let Ok(backup) = std::env::var("GNOSIS_TEST_RESTORE_BACKUP") else {
            return;
        };
        let database = std::env::var("GNOSIS_TEST_RESTORE_DATABASE").unwrap();
        let blobs = std::env::var("GNOSIS_TEST_RESTORE_BLOBS").unwrap();
        let interrupted_after = std::env::var("GNOSIS_TEST_RESTORE_STEP").unwrap();
        let result = restore_backup_with_hooks(
            Path::new(&backup),
            Path::new(&database),
            Path::new(&blobs),
            |_| Ok(()),
            |step| {
                if format!("{step:?}") == interrupted_after {
                    std::process::exit(91);
                }
            },
        );
        panic!("restore reached unexpected result before interruption: {result:?}");
    }

    #[test]
    fn relative_restore_target_resolves_beneath_current_directory() {
        let current = fs::canonicalize(".").unwrap();
        assert_eq!(
            canonical_restore_target(Path::new("relative-restore.sqlite")).unwrap(),
            current.join("relative-restore.sqlite")
        );
    }

    #[test]
    fn restore_rejects_backups_at_or_beneath_staging_without_mutation() {
        for alias in ["direct", "parent-components", "symlink"] {
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
            let (backup, backup_argument) = match alias {
                "direct" => (staging.clone(), staging.clone()),
                "parent-components" => {
                    fs::create_dir(&staging).unwrap();
                    fs::create_dir(staging.join("pivot")).unwrap();
                    let backup = staging.join("nested-backup");
                    let argument = staging.join("pivot/../nested-backup");
                    (backup, argument)
                }
                "symlink" => {
                    fs::create_dir(&staging).unwrap();
                    let backup = staging.join("nested-backup");
                    let argument = directory.path().join("backup-link");
                    symlink(&backup, &argument).unwrap();
                    (backup, argument)
                }
                _ => unreachable!(),
            };
            fs::rename(source, &backup).unwrap();

            assert!(matches!(
                restore_backup(&backup_argument, &database, &blob_root),
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
