use std::{path::Path, sync::LazyLock};

use regex::Regex;
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    domain::{DocumentId, DocumentProjection},
    parser::org::{OrgParseError, parse_org},
};

static FILE_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[\[file:([^]\n]+)").expect("file-link regex is valid"));

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreparedNote {
    pub(crate) document_id: DocumentId,
    pub(crate) logical_path: String,
    pub(crate) path_key: String,
    pub(crate) org: String,
    pub(crate) blob_sha256: String,
    pub(crate) projection: DocumentProjection,
    pub(crate) expected_revision: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreparedAsset {
    pub(crate) logical_path: String,
    pub(crate) path_key: String,
    pub(crate) original_name: String,
    pub(crate) expected_revision: Option<u64>,
}

pub(crate) fn prepare_asset(
    logical_path: String,
    original_name: String,
    expected_revision: Option<u64>,
) -> Result<PreparedAsset, ContentError> {
    validate_asset_path(&logical_path)?;
    let name = Path::new(&original_name)
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(ContentError::Path)?;
    if name != original_name {
        return Err(ContentError::Path);
    }
    Ok(PreparedAsset {
        path_key: path_key(&logical_path),
        logical_path,
        original_name,
        expected_revision,
    })
}

pub(crate) fn prepare_note(
    document_id: Option<String>,
    logical_path: String,
    org: String,
    expected_revision: Option<u64>,
) -> Result<PreparedNote, ContentError> {
    validate_org_path(&logical_path)?;
    validate_file_links(&logical_path, &org)?;
    let projection = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| parse_org(&org)))
        .map_err(|_| ContentError::OrgParser)?
        .map_err(ContentError::Org)?;
    let document_id = document_id.unwrap_or_else(|| Uuid::now_v7().to_string());
    Ok(PreparedNote {
        document_id: DocumentId::new(document_id).map_err(|_| ContentError::DocumentId)?,
        path_key: path_key(&logical_path),
        blob_sha256: format!("{:x}", Sha256::digest(org.as_bytes())),
        logical_path,
        org,
        projection,
        expected_revision,
    })
}

pub(crate) fn validate_logical_path(value: &str) -> Result<(), ContentError> {
    if value.is_empty() || value.contains('\0') || value.contains('\\') {
        return Err(ContentError::Path);
    }
    let path = Path::new(value);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(ContentError::Path);
    }
    Ok(())
}

pub(crate) fn validate_org_path(value: &str) -> Result<(), ContentError> {
    validate_logical_path(value)?;
    if !value.ends_with(".org") || value.starts_with("assets/") {
        return Err(ContentError::UnsupportedType);
    }
    Ok(())
}

pub(crate) fn validate_asset_path(value: &str) -> Result<(), ContentError> {
    validate_logical_path(value)?;
    let parts = value.split('/').collect::<Vec<_>>();
    if parts.len() != 3
        || parts[0] != "assets"
        || parts[1].len() != 64
        || !parts[1]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || !supported_asset(parts[2])
    {
        return Err(ContentError::UnsupportedType);
    }
    Ok(())
}

pub(crate) fn canonical_asset_path(
    digest: &str,
    original_name: &str,
) -> Result<String, ContentError> {
    let name = Path::new(original_name)
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(ContentError::Path)?;
    let path = format!("assets/{digest}/{name}");
    validate_asset_path(&path)?;
    Ok(path)
}

pub(crate) fn path_key(value: &str) -> String {
    value.to_lowercase()
}

fn validate_file_links(document_path: &str, org: &str) -> Result<(), ContentError> {
    let _ = document_path;
    for captures in FILE_LINK.captures_iter(org) {
        let target = captures.get(1).ok_or(ContentError::Path)?.as_str();
        let target = target.split("::").next().ok_or(ContentError::Path)?;
        validate_reference(target)?;
    }
    Ok(())
}

fn validate_reference(target: &str) -> Result<(), ContentError> {
    if target.starts_with("http://") || target.starts_with("https://") {
        return Ok(());
    }
    validate_logical_path(target)?;
    if target.starts_with("assets/") {
        return validate_asset_path(target);
    }
    if target.ends_with(".org") {
        Ok(())
    } else {
        Err(ContentError::UnsupportedType)
    }
}

pub(crate) fn supported_asset(value: &str) -> bool {
    let extension = Path::new(value)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        extension.as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "pdf" | "mp3" | "ogg" | "wav" | "mp4"
    )
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub(crate) enum ContentError {
    #[error("invalid document ID")]
    DocumentId,
    #[error("logical path is invalid")]
    Path,
    #[error("file type is not supported")]
    UnsupportedType,
    #[error("invalid Org document")]
    Org(#[from] OrgParseError),
    #[error("Org parser rejected the document")]
    OrgParser,
}

#[cfg(test)]
mod tests {
    use super::{ContentError, canonical_asset_path, prepare_note, validate_asset_path};

    #[test]
    fn rejects_escaping_absolute_and_unsupported_links() {
        for target in ["../outside.png", "/etc/passwd", "image.bmp"] {
            let org = format!("#+TITLE: Test\n[[file:{target}]]\n");
            assert!(prepare_note(None, "notes/test.org".into(), org, None).is_err());
        }
    }

    #[test]
    fn accepts_org_and_canonical_asset_links() {
        let digest = "a".repeat(64);
        let asset = canonical_asset_path(&digest, "image.png").unwrap();
        let org = format!("#+TITLE: Test\n[[file:other.org]]\n[[file:{asset}]]\n");
        let note = prepare_note(None, "notes/test.org".into(), org, None).unwrap();
        assert_eq!(note.logical_path, "notes/test.org");
        assert!(validate_asset_path(&asset).is_ok());
    }

    #[test]
    fn canonical_assets_require_lowercase_sha256() {
        let error = validate_asset_path(&format!("assets/{}/x.png", "A".repeat(64))).unwrap_err();
        assert_eq!(error, ContentError::UnsupportedType);
    }
}
