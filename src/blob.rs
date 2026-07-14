use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use sha2::{Digest, Sha256};
use thiserror::Error;

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
        fs::remove_file(&temporary)?;
        Ok(BlobMeta {
            sha256,
            size_bytes: bytes.len() as u64,
            media_type: media_type.to_owned(),
        })
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
    if digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
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
}
