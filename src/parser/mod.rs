use std::{collections::BTreeSet, path::Path, sync::LazyLock};

use regex::Regex;
use thiserror::Error;

use crate::domain::DocumentProjection;

pub mod org;

pub(crate) const ORG_CONTENT_TYPE: &str = "text/org; charset=utf-8";
pub(crate) const MAX_DOCUMENT_LOGICAL_PATH_BYTES: usize = 4096;

struct DocumentAdapter {
    content_type: &'static str,
    parse: fn(&str) -> Result<DocumentProjection, org::OrgParseError>,
    validate_path: fn(&str) -> Result<(), DocumentParseError>,
    asset_paths: fn(&str) -> Result<Vec<String>, DocumentParseError>,
    retained_asset_paths: fn(&str) -> Vec<String>,
}

static DOCUMENT_ADAPTERS: [DocumentAdapter; 1] = [DocumentAdapter {
    content_type: ORG_CONTENT_TYPE,
    parse: org::parse_org,
    validate_path: validate_org_path,
    asset_paths: org_asset_paths,
    retained_asset_paths: retained_org_asset_paths,
}];

pub(crate) const SUPPORTED_CONTENT_TYPES: [&str; 1] = [ORG_CONTENT_TYPE];

static ORG_FILE_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[{1,2}file:([^]\n]+)").expect("Org file-link regex is valid"));
static ORG_PLAIN_FILE_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|[[:space:](])file:([^[:space:]\[\]<>\n]+)")
        .expect("plain Org file-link regex is valid")
});

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ParsedDocument {
    projection: DocumentProjection,
    asset_paths: Vec<String>,
}

impl ParsedDocument {
    pub(crate) fn into_projection(self) -> DocumentProjection {
        self.projection
    }

    pub(crate) fn asset_paths(&self) -> &[String] {
        &self.asset_paths
    }
}

pub(crate) fn parse_document(
    content_type: &str,
    source: &str,
) -> Result<ParsedDocument, DocumentParseError> {
    let adapter = adapter(content_type)?;
    let projection = (adapter.parse)(source)?;
    let asset_paths = (adapter.asset_paths)(source)?;
    Ok(ParsedDocument {
        projection,
        asset_paths,
    })
}

pub(crate) fn validate_document_path(
    content_type: &str,
    logical_path: &str,
) -> Result<(), DocumentParseError> {
    if logical_path.len() > MAX_DOCUMENT_LOGICAL_PATH_BYTES {
        return Err(DocumentParseError::InvalidReference);
    }
    let adapter = adapter(content_type)?;
    (adapter.validate_path)(logical_path).map_err(|_| DocumentParseError::InvalidReference)
}

pub(crate) fn supported_content_type(content_type: &str) -> bool {
    adapter(content_type).is_ok()
}

fn adapter(content_type: &str) -> Result<&'static DocumentAdapter, DocumentParseError> {
    DOCUMENT_ADAPTERS
        .iter()
        .find(|adapter| adapter.content_type == content_type)
        .ok_or_else(|| DocumentParseError::UnsupportedContentType(content_type.to_owned()))
}

fn org_asset_paths(source: &str) -> Result<Vec<String>, DocumentParseError> {
    let mut paths = BTreeSet::new();
    for expression in [&*ORG_FILE_LINK, &*ORG_PLAIN_FILE_LINK] {
        for captures in expression.captures_iter(source) {
            let target = captures
                .get(1)
                .map(|capture| capture.as_str())
                .ok_or(DocumentParseError::InvalidReference)?;
            let target = target.split("::").next().unwrap_or(target);
            validate_org_reference(target)?;
            if target.starts_with("assets/") {
                paths.insert(target.to_owned());
            }
        }
    }
    Ok(paths.into_iter().collect())
}

pub(crate) fn retained_asset_paths(content_type: &str, source: &str) -> Vec<String> {
    DOCUMENT_ADAPTERS
        .iter()
        .find(|adapter| adapter.content_type == content_type)
        .map(|adapter| (adapter.retained_asset_paths)(source))
        .unwrap_or_default()
}

fn retained_org_asset_paths(source: &str) -> Vec<String> {
    let mut paths = BTreeSet::new();
    for expression in [&*ORG_FILE_LINK, &*ORG_PLAIN_FILE_LINK] {
        for captures in expression.captures_iter(source) {
            let Some(target) = captures.get(1).map(|capture| capture.as_str()) else {
                continue;
            };
            let target = target.split("::").next().unwrap_or(target);
            if target.starts_with("assets/") && valid_asset_path(target) {
                paths.insert(target.to_owned());
            }
        }
    }
    paths.into_iter().collect()
}

fn validate_org_reference(target: &str) -> Result<(), DocumentParseError> {
    if target.starts_with("http://") || target.starts_with("https://") {
        return Ok(());
    }
    if !valid_relative_path(target) {
        return Err(DocumentParseError::InvalidReference);
    }
    if target.starts_with("assets/") {
        return valid_asset_path(target)
            .then_some(())
            .ok_or(DocumentParseError::InvalidReference);
    }
    target
        .ends_with(".org")
        .then_some(())
        .ok_or(DocumentParseError::InvalidReference)
}

fn validate_org_path(value: &str) -> Result<(), DocumentParseError> {
    if valid_relative_path(value) && value.ends_with(".org") && !value.starts_with("assets/") {
        Ok(())
    } else {
        Err(DocumentParseError::InvalidReference)
    }
}

fn valid_relative_path(value: &str) -> bool {
    !value.is_empty()
        && !value.contains('\0')
        && !value.contains('\\')
        && !Path::new(value).is_absolute()
        && Path::new(value)
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
}

fn valid_asset_path(value: &str) -> bool {
    let parts = value.split('/').collect::<Vec<_>>();
    parts.len() == 3
        && parts[0] == "assets"
        && crate::is_canonical_sha256(parts[1])
        && supported_asset(parts[2])
}

fn supported_asset(value: &str) -> bool {
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
pub(crate) enum DocumentParseError {
    #[error("unsupported document content type: {0}")]
    UnsupportedContentType(String),
    #[error("invalid Org document")]
    Org(#[from] org::OrgParseError),
    #[error("invalid Org reference")]
    InvalidReference,
}

#[cfg(test)]
mod org_tests;

#[cfg(test)]
mod tests {
    use super::{
        DocumentParseError, ORG_CONTENT_TYPE, SUPPORTED_CONTENT_TYPES, parse_document,
        validate_document_path,
    };

    #[test]
    fn dispatches_supported_content_type_to_org_adapter() {
        let parsed = parse_document(ORG_CONTENT_TYPE, "#+title: Note\n").unwrap();

        assert!(parsed.asset_paths().is_empty());
        assert_eq!(parsed.into_projection().title(), "Note");
        assert_eq!(SUPPORTED_CONTENT_TYPES, [ORG_CONTENT_TYPE]);
    }

    #[test]
    fn rejects_unsupported_content_type_without_parsing_source() {
        let error = parse_document("text/markdown; charset=utf-8", "# Note").unwrap_err();

        assert_eq!(
            error,
            DocumentParseError::UnsupportedContentType("text/markdown; charset=utf-8".into())
        );
    }

    #[test]
    fn org_adapter_owns_logical_path_policy() {
        assert!(validate_document_path(ORG_CONTENT_TYPE, "notes/topic.org").is_ok());
        assert_eq!(
            validate_document_path(ORG_CONTENT_TYPE, "notes/topic.md"),
            Err(DocumentParseError::InvalidReference)
        );
    }
}
