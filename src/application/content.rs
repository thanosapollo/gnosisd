use std::{collections::BTreeSet, path::Path, sync::LazyLock};

use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    application::offline_asset::MAX_OFFLINE_ASSET_BYTES,
    domain::{
        DocumentId, DocumentProjection, NodeId, ThemaConcept, ThemaConceptRelation, ThemaContent,
        ThemaContentError, ThemaId, ThemaKind,
    },
    is_canonical_sha256,
    parser::{DocumentParseError, org::OrgParseError, parse_document},
};

pub(crate) const MAX_DOCUMENT_SOURCE_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_DOCUMENT_PROJECTION_BYTES: usize = 1024 * 1024;

static FILE_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[{1,2}file:([^]\n]+)").expect("file-link regex is valid"));
static PLAIN_FILE_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|[[:space:](])file:([^[:space:]\[\]<>\n]+)")
        .expect("plain file-link regex is valid")
});

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreparedNote {
    pub(crate) document_id: DocumentId,
    pub(crate) explicit_document_id: bool,
    pub(crate) logical_path_is_request_input: bool,
    pub(crate) logical_path: String,
    pub(crate) path_key: String,
    pub(crate) content_type: String,
    pub(crate) source: String,
    pub(crate) blob_sha256: String,
    pub(crate) asset_paths: Vec<String>,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreparedThema {
    pub(crate) thema_id: ThemaId,
    pub(crate) content: ThemaContent,
    pub(crate) tags: Vec<String>,
    pub(crate) concepts: Vec<ThemaConcept>,
    pub(crate) asset_paths: Vec<String>,
    pub(crate) content_sha256: String,
    pub(crate) expected_revision: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ContentChange {
    pub(crate) cursor: u64,
    pub(crate) feed_token: String,
    pub(crate) entity_kind: String,
    pub(crate) entity_id: String,
    pub(crate) revision: u64,
    pub(crate) operation: String,
    pub(crate) created_at_us: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DocumentHead {
    pub(crate) document_id: String,
    pub(crate) logical_path: String,
    pub(crate) content_type: String,
    pub(crate) revision: u64,
    pub(crate) blob_sha256: Option<String>,
    pub(crate) active: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct DocumentRevision {
    pub(crate) document_id: String,
    pub(crate) revision: u64,
    pub(crate) logical_path: String,
    pub(crate) content_type: String,
    pub(crate) blob_sha256: Option<String>,
    pub(crate) operation: String,
    pub(crate) created_at_us: u64,
    pub(crate) author_device_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectionNode {
    pub(crate) node_id: String,
    pub(crate) title: String,
    pub(crate) title_path: Vec<String>,
    pub(crate) tags: Vec<String>,
    pub(crate) parent_id: Option<String>,
    pub(crate) level: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectionLinkOccurrence {
    pub(crate) owner_id: String,
    pub(crate) target_id: String,
    pub(crate) start: usize,
    pub(crate) end: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectionEdge {
    pub(crate) source_id: String,
    pub(crate) target_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DocumentProjectionSnapshot {
    pub(crate) title: String,
    pub(crate) file_node_id: Option<String>,
    pub(crate) file_tags: Vec<String>,
    pub(crate) nodes: Vec<ProjectionNode>,
    pub(crate) link_occurrences: Vec<ProjectionLinkOccurrence>,
    pub(crate) semantic_edges: Vec<ProjectionEdge>,
    pub(crate) content_sha256: String,
}

impl From<&DocumentProjection> for DocumentProjectionSnapshot {
    fn from(value: &DocumentProjection) -> Self {
        Self {
            title: value.title().to_owned(),
            file_node_id: value.file_node_id().map(ToString::to_string),
            file_tags: value.file_tags().to_vec(),
            nodes: value
                .nodes()
                .iter()
                .map(|node| ProjectionNode {
                    node_id: node.id().to_string(),
                    title: node.title().to_owned(),
                    title_path: node.title_path().parts().to_vec(),
                    tags: node.tags().to_vec(),
                    parent_id: node.parent_id().map(ToString::to_string),
                    level: node.level(),
                })
                .collect(),
            link_occurrences: value
                .link_occurrences()
                .iter()
                .map(|link| ProjectionLinkOccurrence {
                    owner_id: link.owner().to_string(),
                    target_id: link.target().to_string(),
                    start: link.span().start(),
                    end: link.span().end(),
                })
                .collect(),
            semantic_edges: value
                .semantic_edges()
                .iter()
                .map(|edge| ProjectionEdge {
                    source_id: edge.source().to_string(),
                    target_id: edge.target().to_string(),
                })
                .collect(),
            content_sha256: value.content_sha256().to_owned(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct DocumentReplicaRevision {
    pub(crate) revision: DocumentRevision,
    pub(crate) projection: Option<DocumentProjectionSnapshot>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AssetHead {
    pub(crate) logical_path: String,
    pub(crate) revision: u64,
    pub(crate) blob_sha256: Option<String>,
    pub(crate) media_type: Option<String>,
    pub(crate) active: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AssetRevision {
    pub(crate) logical_path: String,
    pub(crate) revision: u64,
    pub(crate) blob_sha256: Option<String>,
    pub(crate) size_bytes: Option<u64>,
    pub(crate) media_type: Option<String>,
    pub(crate) operation: String,
    pub(crate) created_at_us: u64,
    pub(crate) author_device_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ThemaRecord {
    pub(crate) thema_id: String,
    pub(crate) revision: u64,
    pub(crate) content: ThemaContent,
    pub(crate) content_sha256: String,
    pub(crate) tags: Vec<String>,
    pub(crate) concepts: Vec<ThemaConcept>,
    pub(crate) asset_paths: Vec<String>,
    pub(crate) archived: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ThemaRevision {
    pub(crate) record: ThemaRecord,
    pub(crate) operation: String,
    pub(crate) created_at_us: u64,
    pub(crate) author_device_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NodeRecord {
    pub(crate) node_id: String,
    pub(crate) document_id: String,
    pub(crate) logical_path: String,
    pub(crate) title: String,
    pub(crate) title_path: Vec<String>,
    pub(crate) tags: Vec<String>,
    pub(crate) level: u32,
    pub(crate) active: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NodeThemaAssociation {
    pub(crate) thema_id: String,
    pub(crate) relation: ThemaConceptRelation,
}

pub(crate) fn prepare_thema(
    thema_id: Option<String>,
    content: ThemaContent,
    tags: Vec<String>,
    concepts: Vec<(String, ThemaConceptRelation)>,
    asset_paths: Vec<String>,
    expected_revision: Option<u64>,
) -> Result<PreparedThema, ContentError> {
    let content = content.validate()?;
    let tags = validated_tags(tags)?;
    let thema_id = ThemaId::new(thema_id.unwrap_or_else(|| Uuid::now_v7().to_string()))
        .map_err(|_| ContentError::ThemaId)?;
    let concepts = validated_concepts(&thema_id, concepts)?;
    let asset_paths = validated_assets(asset_paths)?;
    validate_thema_references(&content, &asset_paths)?;
    let content_sha256 = thema_digest(&content, &tags, &concepts, &asset_paths)?;
    Ok(PreparedThema {
        thema_id,
        content,
        tags,
        concepts,
        asset_paths,
        content_sha256,
        expected_revision,
    })
}

pub(crate) fn thema_content(
    kind: &str,
    keimenon: String,
    hypothesis: Vec<Option<String>>,
    answer: Vec<String>,
    source_guid: Option<String>,
    parathema: Option<String>,
    review_image: Option<String>,
) -> Result<ThemaContent, ContentError> {
    Ok(ThemaContent {
        kind: ThemaKind::parse(kind)?,
        keimenon,
        hypothesis,
        answer,
        source_guid,
        parathema,
        review_image,
    }
    .validate()?)
}

pub(crate) fn stored_thema_content(
    kind: &str,
    keimenon: String,
    hypothesis: Vec<Option<String>>,
    answer: Vec<String>,
    source_guid: Option<String>,
    parathema: Option<String>,
    review_image: Option<String>,
) -> Result<ThemaContent, ContentError> {
    if answer.is_empty() {
        return Err(ContentError::Thema(ThemaContentError::Blank));
    }
    Ok(ThemaContent {
        kind: ThemaKind::parse(kind)?,
        keimenon,
        hypothesis,
        answer,
        source_guid,
        parathema,
        review_image,
    })
}

fn validated_tags(values: Vec<String>) -> Result<Vec<String>, ContentError> {
    if values.iter().any(|value| value.trim().is_empty()) {
        return Err(ContentError::Tag);
    }
    Ok(values
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect())
}

fn validated_concepts(
    thema_id: &ThemaId,
    values: Vec<(String, ThemaConceptRelation)>,
) -> Result<Vec<ThemaConcept>, ContentError> {
    let mut concepts = std::collections::BTreeMap::new();
    for (node_id, relation) in values {
        let node_id = NodeId::new(node_id).map_err(|_| ContentError::NodeId)?;
        if concepts
            .insert(node_id.as_str().to_owned(), relation)
            .is_some_and(|current| current != relation)
        {
            return Err(ContentError::ConceptRelation);
        }
    }
    concepts
        .into_iter()
        .map(|(node_id, relation)| {
            Ok(ThemaConcept::new(
                thema_id.clone(),
                NodeId::new(node_id).map_err(|_| ContentError::NodeId)?,
                relation,
            ))
        })
        .collect()
}

fn validated_assets(values: Vec<String>) -> Result<Vec<String>, ContentError> {
    values
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|value| {
            validate_asset_path(&value)?;
            Ok(value)
        })
        .collect()
}

fn validate_thema_references(
    content: &ThemaContent,
    asset_paths: &[String],
) -> Result<(), ContentError> {
    let text = std::iter::once(content.keimenon.as_str())
        .chain(content.hypothesis.iter().filter_map(Option::as_deref))
        .chain(content.answer.iter().map(String::as_str))
        .chain(content.parathema.as_deref());
    for value in text {
        for expression in file_link_expressions() {
            for captures in expression.captures_iter(value) {
                let target = captures.get(1).ok_or(ContentError::Path)?.as_str();
                validate_thema_asset_reference(
                    target.split("::").next().ok_or(ContentError::Path)?,
                    asset_paths,
                )?;
            }
        }
    }
    if let Some(reference) = content.review_image.as_deref() {
        validate_thema_asset_reference(reference, asset_paths)?;
    }
    Ok(())
}

pub(crate) fn validate_offline_thema_assets(content: &ThemaContent) -> Result<(), ContentError> {
    let text = std::iter::once(content.keimenon.as_str())
        .chain(content.hypothesis.iter().filter_map(Option::as_deref))
        .chain(content.answer.iter().map(String::as_str))
        .chain(content.parathema.as_deref());
    for value in text {
        for expression in file_link_expressions() {
            for captures in expression.captures_iter(value) {
                let target = captures.get(1).ok_or(ContentError::Path)?.as_str();
                let target = target.split("::").next().ok_or(ContentError::Path)?;
                if external_reference(target) {
                    return Err(ContentError::UnmanagedAsset);
                }
            }
        }
    }
    if content
        .review_image
        .as_deref()
        .is_some_and(external_reference)
    {
        return Err(ContentError::UnmanagedAsset);
    }
    Ok(())
}

fn validate_thema_asset_reference(
    reference: &str,
    asset_paths: &[String],
) -> Result<(), ContentError> {
    if external_reference(reference) {
        return Ok(());
    }
    validate_asset_path(reference)?;
    if asset_paths
        .binary_search_by(|path| path.as_str().cmp(reference))
        .is_err()
    {
        return Err(ContentError::UnmanagedAsset);
    }
    Ok(())
}

fn thema_digest(
    content: &ThemaContent,
    tags: &[String],
    concepts: &[ThemaConcept],
    assets: &[String],
) -> Result<String, ContentError> {
    let canonical = serde_json::to_vec(&serde_json::json!({
        "answer": content.answer,
        "assets": assets,
        "hypothesis": content.hypothesis,
        "keimenon": content.keimenon,
        "kind": content.kind.as_str(),
        "concepts": concepts.iter().map(|concept| serde_json::json!({
            "node_id": concept.concept_id().as_str(),
            "relation": concept.relation().as_str(),
        })).collect::<Vec<_>>(),
        "parathema": content.parathema,
        "review_image": content.review_image,
        "source_guid": content.source_guid,
        "tags": tags,
    }))
    .map_err(|_| ContentError::Serialization)?;
    Ok(format!("{:x}", Sha256::digest(canonical)))
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

#[cfg(test)]
pub(crate) fn prepare_note(
    document_id: Option<String>,
    logical_path: String,
    org: String,
    expected_revision: Option<u64>,
) -> Result<PreparedNote, ContentError> {
    validate_org_path(&logical_path)?;
    prepare_document(
        document_id,
        logical_path,
        crate::parser::ORG_CONTENT_TYPE.to_owned(),
        org,
        expected_revision,
    )
}

pub(crate) fn prepare_document(
    document_id: Option<String>,
    logical_path: String,
    content_type: String,
    source: String,
    expected_revision: Option<u64>,
) -> Result<PreparedNote, ContentError> {
    validate_document_path(&logical_path)?;
    if source.len() > MAX_DOCUMENT_SOURCE_BYTES {
        return Err(ContentError::SourceTooLarge);
    }
    let parsed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        parse_document(&content_type, &source)
    }))
    .map_err(|_| ContentError::DocumentParser)?
    .map_err(|error| match error {
        DocumentParseError::UnsupportedContentType(_) | DocumentParseError::InvalidReference => {
            ContentError::UnsupportedType
        }
        DocumentParseError::Org(error) => ContentError::Org(error),
    })?;
    let asset_paths = parsed.asset_paths().to_vec();
    let projection = parsed.into_projection();
    let projection_bytes = serde_json::to_vec(&DocumentProjectionSnapshot::from(&projection))
        .map_err(|_| ContentError::Serialization)?;
    if projection_bytes.len() > MAX_DOCUMENT_PROJECTION_BYTES {
        return Err(ContentError::ProjectionTooLarge);
    }
    let explicit_document_id = document_id.is_some();
    let document_id = document_id.unwrap_or_else(|| Uuid::now_v7().to_string());
    Ok(PreparedNote {
        document_id: DocumentId::new(document_id).map_err(|_| ContentError::DocumentId)?,
        explicit_document_id,
        logical_path_is_request_input: true,
        path_key: path_key(&logical_path),
        blob_sha256: format!("{:x}", Sha256::digest(source.as_bytes())),
        logical_path,
        content_type,
        source,
        asset_paths,
        projection,
        expected_revision,
    })
}

pub(crate) fn validate_document_path(value: &str) -> Result<(), ContentError> {
    validate_logical_path(value)?;
    if value.starts_with("assets/") {
        return Err(ContentError::UnsupportedType);
    }
    Ok(())
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
        || !is_canonical_sha256(parts[1])
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

pub(crate) fn validate_blob_metadata(
    size_bytes: u64,
    media_type: &str,
) -> Result<(), ContentError> {
    let valid_media = !media_type.is_empty()
        && media_type.len() <= 255
        && media_type
            .bytes()
            .all(|byte| byte.is_ascii_graphic() || byte == b' ')
        && media_type.trim() == media_type;
    if size_bytes == 0 || size_bytes > MAX_OFFLINE_ASSET_BYTES || !valid_media {
        Err(ContentError::BlobMetadata)
    } else {
        Ok(())
    }
}

pub(crate) fn path_key(value: &str) -> String {
    value.to_lowercase()
}

pub(crate) fn asset_paths_in_org(org: &str) -> Result<Vec<String>, ContentError> {
    parse_document(crate::parser::ORG_CONTENT_TYPE, org)
        .map(|parsed| parsed.asset_paths().to_vec())
        .map_err(|error| match error {
            DocumentParseError::UnsupportedContentType(_)
            | DocumentParseError::InvalidReference => ContentError::UnsupportedType,
            DocumentParseError::Org(error) => ContentError::Org(error),
        })
}

pub(crate) fn retained_asset_paths_in_org(org: &str) -> Vec<String> {
    crate::parser::retained_asset_paths(crate::parser::ORG_CONTENT_TYPE, org)
}

fn external_reference(value: &str) -> bool {
    value.starts_with("http://") || value.starts_with("https://")
}

pub(crate) fn file_link_expressions() -> [&'static Regex; 2] {
    [&FILE_LINK, &PLAIN_FILE_LINK]
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
    #[error("invalid thema ID")]
    ThemaId,
    #[error("invalid node ID")]
    NodeId,
    #[error("a node cannot have multiple thema concept relations")]
    ConceptRelation,
    #[error("invalid tag")]
    Tag,
    #[error("logical path is invalid")]
    Path,
    #[error("blob metadata is invalid")]
    BlobMetadata,
    #[error("file type is not supported")]
    UnsupportedType,
    #[error("document source exceeds the size limit")]
    SourceTooLarge,
    #[error("document projection exceeds the size limit")]
    ProjectionTooLarge,
    #[error("thema content references an asset that is not attached")]
    UnmanagedAsset,
    #[error("invalid Org document")]
    Org(#[from] OrgParseError),
    #[error("document parser rejected the document")]
    DocumentParser,
    #[error("thema content is invalid")]
    Thema(#[from] ThemaContentError),
    #[error("content serialization failed")]
    Serialization,
}

#[cfg(test)]
mod tests {
    use crate::domain::ThemaConceptRelation;

    use super::{
        ContentError, MAX_DOCUMENT_SOURCE_BYTES, MAX_OFFLINE_ASSET_BYTES, canonical_asset_path,
        prepare_document, prepare_note, prepare_thema, thema_content, validate_asset_path,
        validate_blob_metadata,
    };

    #[test]
    fn document_preparation_is_content_type_driven_and_source_bounded() {
        let document = prepare_document(
            None,
            "notes/note.org".into(),
            "text/org; charset=utf-8".into(),
            "#+title: Note\n".into(),
            None,
        )
        .unwrap();
        assert_eq!(document.content_type, "text/org; charset=utf-8");
        assert_eq!(document.source, "#+title: Note\n");

        assert_eq!(
            prepare_document(
                None,
                "notes/note.md".into(),
                "text/markdown; charset=utf-8".into(),
                "# Note".into(),
                None,
            )
            .unwrap_err(),
            ContentError::UnsupportedType
        );
        assert_eq!(
            prepare_document(
                None,
                "notes/note.org".into(),
                "text/org; charset=utf-8".into(),
                "x".repeat(MAX_DOCUMENT_SOURCE_BYTES + 1),
                None,
            )
            .unwrap_err(),
            ContentError::SourceTooLarge
        );
    }

    #[test]
    fn blob_metadata_matches_canonical_asset_limits() {
        assert!(validate_blob_metadata(5, "image/png").is_ok());
        for (size_bytes, media_type) in [
            (0, "image/png"),
            (5, ""),
            (5, " image/png"),
            (5, "image/png\n"),
        ] {
            assert_eq!(
                validate_blob_metadata(size_bytes, media_type),
                Err(ContentError::BlobMetadata)
            );
        }
        assert_eq!(
            validate_blob_metadata(MAX_OFFLINE_ASSET_BYTES + 1, "image/png"),
            Err(ContentError::BlobMetadata)
        );
    }

    #[test]
    fn rejects_escaping_absolute_and_unsupported_links() {
        for target in ["../outside.png", "/etc/passwd", "image.bmp"] {
            for link in [format!("[[file:{target}]]"), format!("file:{target}")] {
                let org = format!("#+TITLE: Test\n{link}\n");
                assert!(prepare_note(None, "notes/test.org".into(), org, None).is_err());
            }
        }
    }

    #[test]
    fn accepts_org_and_canonical_asset_links() {
        let digest = "a".repeat(64);
        let asset = canonical_asset_path(&digest, "image.png").unwrap();
        let org = format!("#+TITLE: Test\n[[file:other.org]]\n[[file:{asset}]]\n");
        let note = prepare_note(None, "notes/test.org".into(), org, None).unwrap();
        assert_eq!(note.logical_path, "notes/test.org");
        assert_eq!(note.asset_paths.as_slice(), std::slice::from_ref(&asset));
        assert!(validate_asset_path(&asset).is_ok());
    }

    #[test]
    fn note_asset_paths_are_unique_and_strip_link_searches() {
        let asset = canonical_asset_path(&"a".repeat(64), "image.png").unwrap();
        let org = format!("#+title: Test\n[[file:{asset}::target]]\nfile:{asset}\n");
        let note = prepare_note(None, "note.org".into(), org, None).unwrap();
        assert_eq!(note.asset_paths, [asset]);
    }

    #[test]
    fn retained_asset_paths_ignore_legacy_local_links() {
        let asset = canonical_asset_path(&"a".repeat(64), "image.png").unwrap();
        let org = format!(
            "#+title: Legacy\n[[file:image.png]]\n[[file:assets/not-canonical.png]]\n[[file:{asset}]]\n"
        );

        assert_eq!(super::retained_asset_paths_in_org(&org), [asset]);
    }

    #[test]
    fn canonical_assets_require_lowercase_sha256() {
        let error = validate_asset_path(&format!("assets/{}/x.png", "A".repeat(64))).unwrap_err();
        assert_eq!(error, ContentError::UnsupportedType);
    }

    #[test]
    fn thema_file_links_must_name_attached_managed_assets() {
        let asset = canonical_asset_path(&"b".repeat(64), "scan.png").unwrap();
        let content = thema_content(
            "basic",
            format!("Question [[file:{asset}]]"),
            vec![],
            vec!["Answer".into()],
            None,
            None,
            Some(asset.clone()),
        )
        .unwrap();
        assert!(prepare_thema(None, content.clone(), vec![], vec![], vec![asset], None).is_ok());
        assert_eq!(
            prepare_thema(None, content, vec![], vec![], vec![], None).unwrap_err(),
            ContentError::UnmanagedAsset
        );
    }

    #[test]
    fn thema_concepts_preserve_relations_and_reject_conflicts() {
        let content = thema_content(
            "basic",
            "Question".into(),
            vec![],
            vec!["Answer".into()],
            None,
            None,
            None,
        )
        .unwrap();
        let prepared = prepare_thema(
            Some("thema".into()),
            content.clone(),
            vec![],
            vec![
                ("supporting".into(), ThemaConceptRelation::Supporting),
                ("primary".into(), ThemaConceptRelation::Primary),
                ("reference".into(), ThemaConceptRelation::Reference),
                ("primary".into(), ThemaConceptRelation::Primary),
            ],
            vec![],
            None,
        )
        .unwrap();
        assert_eq!(
            prepared
                .concepts
                .iter()
                .map(|concept| (concept.concept_id().as_str(), concept.relation()))
                .collect::<Vec<_>>(),
            [
                ("primary", ThemaConceptRelation::Primary),
                ("reference", ThemaConceptRelation::Reference),
                ("supporting", ThemaConceptRelation::Supporting),
            ]
        );

        let changed_relation = prepare_thema(
            Some("thema".into()),
            content.clone(),
            vec![],
            vec![("primary".into(), ThemaConceptRelation::Reference)],
            vec![],
            None,
        )
        .unwrap();
        assert_ne!(prepared.content_sha256, changed_relation.content_sha256);
        assert_eq!(
            prepare_thema(
                Some("thema".into()),
                content,
                vec![],
                vec![
                    ("primary".into(), ThemaConceptRelation::Primary),
                    ("primary".into(), ThemaConceptRelation::Supporting),
                ],
                vec![],
                None,
            )
            .unwrap_err(),
            ContentError::ConceptRelation
        );
    }

    #[test]
    fn thema_external_references_remain_external() {
        let content = thema_content(
            "basic",
            "Question".into(),
            vec![],
            vec!["Answer".into()],
            None,
            None,
            Some("https://example.test/image.png".into()),
        )
        .unwrap();
        assert!(prepare_thema(None, content.clone(), vec![], vec![], vec![], None).is_ok());
        assert_eq!(
            super::validate_offline_thema_assets(&content).unwrap_err(),
            ContentError::UnmanagedAsset
        );
    }
}
