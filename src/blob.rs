use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::is_canonical_sha256;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BlobMeta {
    pub(crate) sha256: String,
    pub(crate) size_bytes: u64,
    pub(crate) media_type: String,
}

#[derive(Clone, Debug)]
pub(crate) struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    pub(crate) fn open(root: impl Into<PathBuf>) -> Result<Self, BlobError> {
        let root = root.into();
        match fs::symlink_metadata(&root) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => return Err(BlobError::InvalidRoot),
            Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(&root)?,
            Err(error) => return Err(error.into()),
        }
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        Ok(Self { root })
    }

    pub(crate) fn put(&self, bytes: &[u8], media_type: &str) -> Result<BlobMeta, BlobError> {
        let sha256 = format!("{:x}", Sha256::digest(bytes));
        let path = self.blob_path(&sha256)?;
        let parent = path.parent().ok_or(BlobError::InvalidDigest)?;
        fs::create_dir_all(parent)?;
        sync_directory(&self.root)?;
        let temporary = self.temporary_path(&sha256)?;
        write_private_file(&temporary, bytes)?;
        match fs::hard_link(&temporary, &path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                verify_file(&path, &sha256, bytes.len() as u64)?;
            }
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                return Err(error.into());
            }
        }
        sync_directory(parent)?;
        fs::remove_file(&temporary)?;
        sync_directory(&self.root)?;
        Ok(BlobMeta {
            sha256,
            size_bytes: bytes.len() as u64,
            media_type: media_type.to_owned(),
        })
    }

    pub(crate) fn contains(&self, digest: &str) -> Result<bool, BlobError> {
        match fs::metadata(self.blob_path(digest)?) {
            Ok(metadata) => Ok(metadata.is_file()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn discard(&self, digest: &str) -> Result<(), BlobError> {
        let path = self.blob_path(digest)?;
        let parent = path.parent().ok_or(BlobError::InvalidDigest)?;
        match fs::remove_file(&path) {
            Ok(()) => {
                sync_directory(parent)?;
                if fs::read_dir(parent)?.next().is_none() {
                    fs::remove_dir(parent)?;
                }
                sync_directory(&self.root)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn read(&self, digest: &str) -> Result<Vec<u8>, BlobError> {
        let path = self.blob_path(digest)?;
        let bytes = fs::read(path)?;
        verify_bytes(&bytes, digest)?;
        Ok(bytes)
    }

    pub(crate) fn verify(&self, digest: &str, size: u64) -> Result<(), BlobError> {
        verify_file(&self.blob_path(digest)?, digest, size)
    }

    pub(crate) fn copy_to(&self, digest: &str, size: u64, root: &Path) -> Result<(), BlobError> {
        let source = self.blob_path(digest)?;
        self.verify(digest, size)?;
        let target = root.join(&digest[..2]).join(digest);
        let parent = target.parent().ok_or(BlobError::InvalidDigest)?;
        fs::create_dir_all(parent)?;
        sync_directory(root)?;
        fs::copy(source, &target)?;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600))?;
        File::open(&target)?.sync_all()?;
        sync_directory(parent)?;
        Ok(())
    }

    pub(crate) fn recover(&self, expected: &[(String, u64)]) -> Result<(), BlobError> {
        let expected = expected.iter().cloned().collect::<BTreeMap<_, _>>();
        let mut found = BTreeSet::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_str().ok_or(BlobError::Corrupt)?;
            if name.starts_with(".tmp-") && entry.file_type()?.is_file() {
                fs::remove_file(path)?;
                continue;
            }
            recover_prefix(&path, name, &expected, &mut found)?;
        }
        for (digest, size) in expected {
            if !found.contains(&digest) {
                return Err(BlobError::Corrupt);
            }
            self.verify(&digest, size)?;
        }
        Ok(())
    }

    fn blob_path(&self, digest: &str) -> Result<PathBuf, BlobError> {
        validate_digest(digest)?;
        Ok(self.root.join(&digest[..2]).join(digest))
    }

    fn temporary_path(&self, digest: &str) -> Result<PathBuf, BlobError> {
        let micros = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| BlobError::Clock)?
            .as_micros();
        Ok(self.root.join(format!(
            ".tmp-{}-{micros}-{}",
            std::process::id(),
            &digest[..12]
        )))
    }
}

fn sync_directory(path: &Path) -> Result<(), BlobError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn recover_prefix(
    path: &Path,
    prefix: &str,
    expected: &BTreeMap<String, u64>,
    found: &mut BTreeSet<String>,
) -> Result<(), BlobError> {
    let metadata = fs::symlink_metadata(path)?;
    if prefix.len() != 2
        || !prefix.bytes().all(|byte| byte.is_ascii_hexdigit())
        || !metadata.file_type().is_dir()
    {
        return Err(BlobError::Corrupt);
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let digest = entry
            .file_name()
            .to_str()
            .ok_or(BlobError::Corrupt)?
            .to_owned();
        validate_digest(&digest)?;
        if &digest[..2] != prefix || !entry.file_type()?.is_file() {
            return Err(BlobError::Corrupt);
        }
        if expected.contains_key(&digest) {
            found.insert(digest);
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    if fs::read_dir(path)?.next().is_none() {
        fs::remove_dir(path)?;
    }
    Ok(())
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), BlobError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(path);
        return Err(error.into());
    }
    Ok(())
}

fn validate_digest(digest: &str) -> Result<(), BlobError> {
    if is_canonical_sha256(digest) {
        Ok(())
    } else {
        Err(BlobError::InvalidDigest)
    }
}

fn verify_file(path: &Path, digest: &str, size: u64) -> Result<(), BlobError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.len() != size {
        return Err(BlobError::Corrupt);
    }
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    if format!("{:x}", hasher.finalize()) == digest {
        Ok(())
    } else {
        Err(BlobError::Corrupt)
    }
}

fn verify_bytes(bytes: &[u8], digest: &str) -> Result<(), BlobError> {
    validate_digest(digest)?;
    if format!("{:x}", Sha256::digest(bytes)) == digest {
        Ok(())
    } else {
        Err(BlobError::Corrupt)
    }
}

#[derive(Debug, Error)]
pub(crate) enum BlobError {
    #[error("blob root is not a directory")]
    InvalidRoot,
    #[error("blob digest is invalid")]
    InvalidDigest,
    #[error("blob content is missing or corrupt")]
    Corrupt,
    #[error("system clock is before the Unix epoch")]
    Clock,
    #[error("blob filesystem operation failed")]
    Io(#[from] io::Error),
}

#[cfg(test)]
mod tests {
    use super::BlobStore;

    #[test]
    fn immutable_blobs_deduplicate_and_verify() {
        let directory = tempfile::tempdir().unwrap();
        let store = BlobStore::open(directory.path().join("blobs")).unwrap();
        let first = store.put(b"same", "text/plain").unwrap();
        let second = store.put(b"same", "text/plain").unwrap();
        assert_eq!(first, second);
        let path = directory
            .path()
            .join("blobs")
            .join(&first.sha256[..2])
            .join(&first.sha256);
        assert_eq!(std::fs::read(path).unwrap(), b"same");
    }

    #[test]
    fn startup_recovery_removes_unreferenced_and_interrupted_blobs() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("blobs");
        let store = BlobStore::open(&root).unwrap();
        let blob = store.put(b"orphan", "text/plain").unwrap();
        std::fs::write(root.join(".tmp-interrupted"), b"partial").unwrap();
        store.recover(&[]).unwrap();
        assert!(!root.join(&blob.sha256[..2]).exists());
        assert!(!root.join(".tmp-interrupted").exists());
    }
}
