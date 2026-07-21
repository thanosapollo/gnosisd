use std::{collections::BTreeSet, sync::LazyLock};

use regex::Regex;
use thiserror::Error;

use crate::domain::DocumentProjection;

pub mod org;

pub(crate) const ORG_CONTENT_TYPE: &str = "text/org; charset=utf-8";

struct DocumentAdapter {
    content_type: &'static str,
    parse: fn(&str) -> Result<DocumentProjection, org::OrgParseError>,
    asset_paths: fn(&str) -> Vec<String>,
}

static DOCUMENT_ADAPTERS: [DocumentAdapter; 1] = [DocumentAdapter {
    content_type: ORG_CONTENT_TYPE,
    parse: org::parse_org,
    asset_paths: org_asset_paths,
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
    let adapter = DOCUMENT_ADAPTERS
        .iter()
        .find(|adapter| adapter.content_type == content_type)
        .ok_or_else(|| DocumentParseError::UnsupportedContentType(content_type.to_owned()))?;
    let projection = (adapter.parse)(source)?;
    let asset_paths = (adapter.asset_paths)(source);
    Ok(ParsedDocument {
        projection,
        asset_paths,
    })
}

fn org_asset_paths(source: &str) -> Vec<String> {
    let mut paths = BTreeSet::new();
    for expression in [&*ORG_FILE_LINK, &*ORG_PLAIN_FILE_LINK] {
        for captures in expression.captures_iter(source) {
            let Some(target) = captures.get(1).map(|capture| capture.as_str()) else {
                continue;
            };
            let target = target.split("::").next().unwrap_or(target);
            if target.starts_with("assets/") {
                paths.insert(target.to_owned());
            }
        }
    }
    paths.into_iter().collect()
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub(crate) enum DocumentParseError {
    #[error("unsupported document content type: {0}")]
    UnsupportedContentType(String),
    #[error("invalid Org document")]
    Org(#[from] org::OrgParseError),
}

#[cfg(test)]
mod org_tests;

#[cfg(test)]
mod tests {
    use super::{DocumentParseError, ORG_CONTENT_TYPE, SUPPORTED_CONTENT_TYPES, parse_document};

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
}
