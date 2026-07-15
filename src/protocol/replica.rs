use std::collections::BTreeMap;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::application::{
    DocumentProjectionSnapshot, ProjectionEdge, ProjectionLinkOccurrence, ProjectionNode,
    ReplicaChange, ReplicaProjection, ReplicaState, ReviewStateSnapshot, SnapshotPage,
    SnapshotPosition, TagRule,
};

pub(crate) const REPLICA_VERSION: u32 = 1;
pub(crate) const MAX_REPLICA_CURSOR_BYTES: usize = 512;
pub(crate) const MAX_REPLICA_PAGE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Error)]
pub(crate) enum ReplicaWireError {
    #[error("replica cursor is invalid")]
    Cursor,
    #[error("replica snapshot cursor is invalid")]
    Snapshot,
    #[error("replica response cannot be encoded")]
    Encode,
    #[error("replica projection exceeds the response budget")]
    PageTooLarge,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct CursorV1 {
    version: u32,
    server_id: String,
    position: u64,
    token: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct SnapshotCursorV1 {
    version: u32,
    snapshot_id: String,
    server_id: String,
    cut_cursor: u64,
    cut_token: String,
    after: u64,
}

pub(crate) fn encode_cursor(
    server_id: &str,
    position: u64,
    token: &str,
) -> Result<String, ReplicaWireError> {
    encode_token(&CursorV1 {
        version: REPLICA_VERSION,
        server_id: server_id.to_owned(),
        position,
        token: token.to_owned(),
    })
}

pub(crate) fn decode_cursor(value: &str) -> Result<(String, u64, String), ReplicaWireError> {
    let cursor: CursorV1 = decode_token(value).map_err(|_| ReplicaWireError::Cursor)?;
    if cursor.version != REPLICA_VERSION {
        return Err(ReplicaWireError::Cursor);
    }
    Ok((cursor.server_id, cursor.position, cursor.token))
}

pub(crate) fn encode_snapshot_cursor(
    position: &SnapshotPosition,
) -> Result<String, ReplicaWireError> {
    encode_token(&SnapshotCursorV1 {
        version: REPLICA_VERSION,
        snapshot_id: position.snapshot_id.clone(),
        server_id: position.server_id.clone(),
        cut_cursor: position.cut_cursor,
        cut_token: position.cut_token.clone(),
        after: position.after,
    })
}

pub(crate) fn decode_snapshot_cursor(value: &str) -> Result<SnapshotPosition, ReplicaWireError> {
    let cursor: SnapshotCursorV1 = decode_token(value).map_err(|_| ReplicaWireError::Snapshot)?;
    if cursor.version != REPLICA_VERSION {
        return Err(ReplicaWireError::Snapshot);
    }
    Ok(SnapshotPosition {
        snapshot_id: cursor.snapshot_id,
        server_id: cursor.server_id,
        cut_cursor: cursor.cut_cursor,
        cut_token: cursor.cut_token,
        after: cursor.after,
    })
}

fn encode_token<T: Serialize>(value: &T) -> Result<String, ReplicaWireError> {
    let json = serde_json::to_vec(value).map_err(|_| ReplicaWireError::Encode)?;
    Ok(URL_SAFE_NO_PAD.encode(json))
}

fn decode_token<T: for<'de> Deserialize<'de>>(value: &str) -> Result<T, ReplicaWireError> {
    if value.len() > MAX_REPLICA_CURSOR_BYTES {
        return Err(ReplicaWireError::Cursor);
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| ReplicaWireError::Cursor)?;
    serde_json::from_slice(&bytes).map_err(|_| ReplicaWireError::Cursor)
}

pub(crate) fn replica_state_json(state: &ReplicaState) -> Result<Value, ReplicaWireError> {
    serde_json::to_value(ReplicaStateV1 {
        version: REPLICA_VERSION,
        server_id: state.server_id.clone(),
        cursor: encode_cursor(&state.server_id, state.cursor, &state.cursor_token)?,
        next_review_sequence: state.next_review_sequence,
        snapshot_required: true,
    })
    .map_err(|_| ReplicaWireError::Encode)
}

pub(crate) fn replica_changes_json(
    server_id: &str,
    from: u64,
    from_token: &str,
    latest: u64,
    changes: &[ReplicaChange],
) -> Result<Value, ReplicaWireError> {
    let items = changes
        .iter()
        .map(|change| ChangeV1::from_change(server_id, change))
        .collect::<Result<Vec<_>, _>>()?;
    encode_change_page(server_id, from, from_token, latest, changes, &items)
}

pub(crate) fn replica_snapshot_json(page: &SnapshotPage) -> Result<Value, ReplicaWireError> {
    let items = page
        .items
        .iter()
        .map(ProjectionV1::from)
        .collect::<Vec<_>>();
    encode_snapshot_page(page, &items)
}

fn encode_change_page(
    server_id: &str,
    from: u64,
    from_token: &str,
    latest: u64,
    changes: &[ReplicaChange],
    items: &[ChangeV1],
) -> Result<Value, ReplicaWireError> {
    for count in (0..=items.len()).rev() {
        let next_change = count.checked_sub(1).and_then(|index| changes.get(index));
        let next = next_change.map_or(from, |change| change.cursor);
        let token = next_change.map_or(from_token, |change| change.cursor_token.as_str());
        let page = ChangePageV1 {
            version: REPLICA_VERSION,
            from: encode_cursor(server_id, from, from_token)?,
            next: encode_cursor(server_id, next, token)?,
            has_more: count < items.len() || next < latest,
            items: &items[..count],
        };
        if let Some(value) = bounded_json(&page)? {
            if count == 0 && !items.is_empty() {
                return Err(ReplicaWireError::PageTooLarge);
            }
            return Ok(value);
        }
    }
    Err(ReplicaWireError::PageTooLarge)
}

fn encode_snapshot_page(
    page: &SnapshotPage,
    items: &[ProjectionV1],
) -> Result<Value, ReplicaWireError> {
    for count in (0..=items.len()).rev() {
        let has_more = count < items.len() || page.next.is_some();
        let next = snapshot_next(page, count, has_more)
            .as_ref()
            .map(encode_snapshot_cursor)
            .transpose()?;
        let cursor = (!has_more)
            .then(|| {
                encode_cursor(
                    &page.position.server_id,
                    page.position.cut_cursor,
                    &page.position.cut_token,
                )
            })
            .transpose()?;
        let wire = SnapshotPageV1 {
            version: REPLICA_VERSION,
            snapshot_id: page.position.snapshot_id.clone(),
            cursor,
            next,
            items: &items[..count],
        };
        if let Some(value) = bounded_json(&wire)? {
            if count == 0 && !items.is_empty() {
                return Err(ReplicaWireError::PageTooLarge);
            }
            return Ok(value);
        }
    }
    Err(ReplicaWireError::PageTooLarge)
}

fn snapshot_next(page: &SnapshotPage, count: usize, has_more: bool) -> Option<SnapshotPosition> {
    if !has_more {
        return None;
    }
    if count < page.items.len() {
        return Some(SnapshotPosition {
            after: page.position.after + count as u64,
            ..page.position.clone()
        });
    }
    page.next.clone()
}

fn bounded_json(value: &impl Serialize) -> Result<Option<Value>, ReplicaWireError> {
    let bytes = serde_json::to_vec(value).map_err(|_| ReplicaWireError::Encode)?;
    if bytes.len() > MAX_REPLICA_PAGE_BYTES {
        return Ok(None);
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| ReplicaWireError::Encode)
}

#[derive(Serialize)]
struct ReplicaStateV1 {
    version: u32,
    server_id: String,
    cursor: String,
    next_review_sequence: u64,
    snapshot_required: bool,
}

#[derive(Serialize)]
struct ChangePageV1<'a> {
    version: u32,
    from: String,
    next: String,
    has_more: bool,
    items: &'a [ChangeV1],
}

#[derive(Serialize)]
struct ChangeV1 {
    cursor: String,
    #[serde(flatten)]
    projection: ProjectionV1,
}

impl ChangeV1 {
    fn from_change(server_id: &str, change: &ReplicaChange) -> Result<Self, ReplicaWireError> {
        Ok(Self {
            cursor: encode_cursor(server_id, change.cursor, &change.cursor_token)?,
            projection: ProjectionV1::from(&change.projection),
        })
    }
}

#[derive(Serialize)]
struct SnapshotPageV1<'a> {
    version: u32,
    snapshot_id: String,
    cursor: Option<String>,
    next: Option<String>,
    items: &'a [ProjectionV1],
}

#[derive(Serialize)]
#[serde(tag = "entity_kind", content = "projection", rename_all = "snake_case")]
enum ProjectionV1 {
    Document(DocumentRevisionV1),
    Asset(AssetRevisionV1),
    Thema(ThemaRevisionV1),
    Settings(SettingsRevisionV1),
    ReviewState(ReviewStateRevisionV1),
}

impl From<&ReplicaProjection> for ProjectionV1 {
    fn from(value: &ReplicaProjection) -> Self {
        match value {
            ReplicaProjection::Document(value) => Self::Document(DocumentRevisionV1 {
                document_id: value.revision.document_id.clone(),
                revision: value.revision.revision,
                logical_path: value.revision.logical_path.clone(),
                content_sha256: value.revision.blob_sha256.clone(),
                blob_url: value.revision.blob_sha256.as_deref().map(blob_url),
                operation: value.revision.operation.clone(),
                created_at_us: value.revision.created_at_us,
                author_device_id: value.revision.author_device_id.clone(),
                projection: value.projection.as_ref().map(DocumentProjectionV1::from),
            }),
            ReplicaProjection::Asset(value) => Self::Asset(AssetRevisionV1 {
                logical_path: value.logical_path.clone(),
                revision: value.revision,
                content_sha256: value.blob_sha256.clone(),
                size_bytes: value.size_bytes,
                media_type: value.media_type.clone(),
                blob_url: value.blob_sha256.as_deref().map(blob_url),
                operation: value.operation.clone(),
                created_at_us: value.created_at_us,
                author_device_id: value.author_device_id.clone(),
            }),
            ReplicaProjection::Thema(value) => Self::Thema(ThemaRevisionV1::from(value)),
            ReplicaProjection::Settings(value) => Self::Settings(SettingsRevisionV1::from(value)),
            ReplicaProjection::ReviewState(value) => Self::ReviewState(ReviewStateRevisionV1 {
                thema_id: value.thema_id.clone(),
                state: ReviewStateV1::from(&value.state),
                event_id: value.event_id.clone(),
                created_at_us: value.created_at_us,
            }),
        }
    }
}

fn blob_url(digest: &str) -> String {
    format!("/v1/blobs/{digest}")
}

#[derive(Serialize)]
struct DocumentRevisionV1 {
    document_id: String,
    revision: u64,
    logical_path: String,
    content_sha256: Option<String>,
    blob_url: Option<String>,
    operation: String,
    created_at_us: u64,
    author_device_id: Option<String>,
    projection: Option<DocumentProjectionV1>,
}

#[derive(Serialize)]
struct DocumentProjectionV1 {
    title: String,
    file_node_id: Option<String>,
    file_tags: Vec<String>,
    nodes: Vec<ProjectionNodeV1>,
    link_occurrences: Vec<ProjectionLinkV1>,
    semantic_edges: Vec<ProjectionEdgeV1>,
    compatibility_sha1: String,
    content_sha256: String,
}

impl From<&DocumentProjectionSnapshot> for DocumentProjectionV1 {
    fn from(value: &DocumentProjectionSnapshot) -> Self {
        Self {
            title: value.title.clone(),
            file_node_id: value.file_node_id.clone(),
            file_tags: value.file_tags.clone(),
            nodes: value.nodes.iter().map(ProjectionNodeV1::from).collect(),
            link_occurrences: value
                .link_occurrences
                .iter()
                .map(ProjectionLinkV1::from)
                .collect(),
            semantic_edges: value
                .semantic_edges
                .iter()
                .map(ProjectionEdgeV1::from)
                .collect(),
            compatibility_sha1: value.compatibility_sha1.clone(),
            content_sha256: value.content_sha256.clone(),
        }
    }
}

#[derive(Serialize)]
struct ProjectionNodeV1 {
    node_id: String,
    title: String,
    title_path: Vec<String>,
    tags: Vec<String>,
    parent_id: Option<String>,
    level: u32,
}

impl From<&ProjectionNode> for ProjectionNodeV1 {
    fn from(value: &ProjectionNode) -> Self {
        Self {
            node_id: value.node_id.clone(),
            title: value.title.clone(),
            title_path: value.title_path.clone(),
            tags: value.tags.clone(),
            parent_id: value.parent_id.clone(),
            level: value.level,
        }
    }
}

#[derive(Serialize)]
struct ProjectionLinkV1 {
    owner_id: String,
    target_id: String,
    start: usize,
    end: usize,
}

impl From<&ProjectionLinkOccurrence> for ProjectionLinkV1 {
    fn from(value: &ProjectionLinkOccurrence) -> Self {
        Self {
            owner_id: value.owner_id.clone(),
            target_id: value.target_id.clone(),
            start: value.start,
            end: value.end,
        }
    }
}

#[derive(Serialize)]
struct ProjectionEdgeV1 {
    source_id: String,
    target_id: String,
}

impl From<&ProjectionEdge> for ProjectionEdgeV1 {
    fn from(value: &ProjectionEdge) -> Self {
        Self {
            source_id: value.source_id.clone(),
            target_id: value.target_id.clone(),
        }
    }
}

#[derive(Serialize)]
struct AssetRevisionV1 {
    logical_path: String,
    revision: u64,
    content_sha256: Option<String>,
    size_bytes: Option<u64>,
    media_type: Option<String>,
    blob_url: Option<String>,
    operation: String,
    created_at_us: u64,
    author_device_id: Option<String>,
}

#[derive(Serialize)]
struct ThemaRevisionV1 {
    thema_id: String,
    revision: u64,
    kind: String,
    keimenon: String,
    hypothesis: Vec<Option<String>>,
    answer: Vec<String>,
    source_guid: Option<String>,
    parathema: Option<String>,
    review_image: Option<String>,
    tags: Vec<String>,
    node_ids: Vec<String>,
    asset_paths: Vec<String>,
    content_sha256: String,
    archived: bool,
    operation: String,
    created_at_us: u64,
    author_device_id: Option<String>,
}

impl From<&crate::application::ThemaRevision> for ThemaRevisionV1 {
    fn from(value: &crate::application::ThemaRevision) -> Self {
        Self {
            thema_id: value.record.thema_id.clone(),
            revision: value.record.revision,
            kind: value.record.content.kind.as_str().to_owned(),
            keimenon: value.record.content.keimenon.clone(),
            hypothesis: value.record.content.hypothesis.clone(),
            answer: value.record.content.answer.clone(),
            source_guid: value.record.content.source_guid.clone(),
            parathema: value.record.content.parathema.clone(),
            review_image: value.record.content.review_image.clone(),
            tags: value.record.tags.clone(),
            node_ids: value.record.node_ids.clone(),
            asset_paths: value.record.asset_paths.clone(),
            content_sha256: value.record.content_sha256.clone(),
            archived: value.record.archived,
            operation: value.operation.clone(),
            created_at_us: value.created_at_us,
            author_device_id: value.author_device_id.clone(),
        }
    }
}

#[derive(Serialize)]
struct SettingsRevisionV1 {
    revision: u64,
    timezone: String,
    day_boundary_hour: u8,
    scheduler: ParametersV1,
    tag_rules: BTreeMap<String, TagRuleV1>,
    operation: String,
    created_at_us: u64,
    author_device_id: Option<String>,
}

impl From<&crate::application::SettingsRevision> for SettingsRevisionV1 {
    fn from(value: &crate::application::SettingsRevision) -> Self {
        Self {
            revision: value.settings.revision,
            timezone: value.settings.timezone.clone(),
            day_boundary_hour: value.settings.day_boundary_hour,
            scheduler: ParametersV1::from(&value.settings.scheduler),
            tag_rules: value
                .settings
                .tag_rules
                .iter()
                .map(|(tag, rule)| (tag.clone(), TagRuleV1::from(rule)))
                .collect(),
            operation: value.operation.clone(),
            created_at_us: value.created_at_us,
            author_device_id: value.author_device_id.clone(),
        }
    }
}

#[derive(Serialize)]
struct ParametersV1 {
    scheduler_version: u32,
    amnesia: f64,
    epignosis: f64,
    agnoia: f64,
    anagnosis: u64,
    lethe: u64,
    synolon_max: f64,
    interval_fuzz: f64,
    proto: Vec<u32>,
}

impl From<&crate::application::ParametersSnapshot> for ParametersV1 {
    fn from(value: &crate::application::ParametersSnapshot) -> Self {
        Self {
            scheduler_version: value.scheduler_version,
            amnesia: value.amnesia,
            epignosis: value.epignosis,
            agnoia: value.agnoia,
            anagnosis: value.anagnosis,
            lethe: value.lethe,
            synolon_max: value.synolon_max,
            interval_fuzz: value.interval_fuzz,
            proto: value.proto.clone(),
        }
    }
}

#[derive(Serialize)]
struct TagRuleV1 {
    amnesia: Option<f64>,
    epignosis: Option<f64>,
    agnoia: Option<f64>,
    anagnosis: Option<u64>,
    lethe: Option<u64>,
    proto: Option<Vec<u32>>,
}

impl From<&TagRule> for TagRuleV1 {
    fn from(value: &TagRule) -> Self {
        Self {
            amnesia: value.amnesia,
            epignosis: value.epignosis,
            agnoia: value.agnoia,
            anagnosis: value.anagnosis,
            lethe: value.lethe,
            proto: value.proto.clone(),
        }
    }
}

#[derive(Serialize)]
struct ReviewStateRevisionV1 {
    thema_id: String,
    state: ReviewStateV1,
    event_id: Option<String>,
    created_at_us: u64,
}

#[derive(Serialize)]
struct ReviewStateV1 {
    state_revision: u64,
    review_count: u64,
    legacy_review_count: u64,
    legacy_unclassified_count: u64,
    last_reviewed_at_us: Option<u64>,
    last_review_day: Option<u32>,
    next_review_day: u32,
    interval_days: Option<u32>,
    consecutive_successes: u64,
    total_successes: u64,
    consecutive_failures: u64,
    total_failures: u64,
    suspended: bool,
    gnosis_plus: f64,
    gnosis_minus: f64,
    gnosis_synolon: f64,
}

impl From<&ReviewStateSnapshot> for ReviewStateV1 {
    fn from(value: &ReviewStateSnapshot) -> Self {
        Self {
            state_revision: value.state_revision,
            review_count: value.review_count,
            legacy_review_count: value.legacy_review_count,
            legacy_unclassified_count: value.legacy_unclassified_count,
            last_reviewed_at_us: value.last_reviewed_at_us,
            last_review_day: value.last_review_day,
            next_review_day: value.next_review_day,
            interval_days: value.interval_days,
            consecutive_successes: value.consecutive_successes,
            total_successes: value.total_successes,
            consecutive_failures: value.consecutive_failures,
            total_failures: value.total_failures,
            suspended: value.suspended,
            gnosis_plus: value.gnosis_plus,
            gnosis_minus: value.gnosis_minus,
            gnosis_synolon: value.gnosis_synolon,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::{Value, json};

    use super::{
        CursorV1, MAX_REPLICA_CURSOR_BYTES, ReplicaWireError, decode_cursor,
        decode_snapshot_cursor, encode_cursor, encode_snapshot_cursor, encode_token,
        replica_changes_json, replica_snapshot_json, replica_state_json,
    };
    use crate::application::{
        AssetRevision, DocumentProjectionSnapshot, DocumentReplicaRevision, DocumentRevision,
        ParametersSnapshot, ProjectionEdge, ProjectionLinkOccurrence, ProjectionNode,
        ReplicaChange, ReplicaProjection, ReplicaState, ReviewStateRevision, ReviewStateSnapshot,
        SettingsRecord, SettingsRevision, SnapshotPage, SnapshotPosition, TagRule, ThemaRecord,
        ThemaRevision, thema_content,
    };

    #[test]
    fn opaque_cursors_round_trip_and_reject_unknown_versions() {
        let cursor = encode_cursor("server", 42, "token").unwrap();
        assert_eq!(
            decode_cursor(&cursor).unwrap(),
            ("server".into(), 42, "token".into())
        );

        let position = SnapshotPosition {
            snapshot_id: "snapshot".into(),
            server_id: "server".into(),
            cut_cursor: 42,
            cut_token: "token".into(),
            after: 7,
        };
        let cursor = encode_snapshot_cursor(&position).unwrap();
        assert_eq!(decode_snapshot_cursor(&cursor).unwrap(), position);

        let future = encode_token(&CursorV1 {
            version: 2,
            server_id: "server".into(),
            position: 42,
            token: "token".into(),
        })
        .unwrap();
        assert!(matches!(
            decode_cursor(&future),
            Err(ReplicaWireError::Cursor)
        ));
        assert!(matches!(
            decode_cursor(&"x".repeat(MAX_REPLICA_CURSOR_BYTES + 1)),
            Err(ReplicaWireError::Cursor)
        ));
    }

    #[test]
    fn v1_state_and_snapshot_match_the_golden_fixture() {
        let position = SnapshotPosition {
            snapshot_id: "snapshot-1".into(),
            server_id: "0123456789abcdef0123456789abcdef".into(),
            cut_cursor: 7,
            cut_token: "11111111111111111111111111111111".into(),
            after: 0,
        };
        let page = SnapshotPage {
            position: position.clone(),
            items: vec![document_projection(), tombstone_projection()],
            next: Some(SnapshotPosition {
                after: 2,
                ..position.clone()
            }),
        };
        let terminal = SnapshotPage {
            position: SnapshotPosition {
                after: 2,
                ..position.clone()
            },
            items: vec![],
            next: None,
        };
        let changes = supplemental_projections()
            .into_iter()
            .enumerate()
            .map(|(index, projection)| ReplicaChange {
                cursor: 8 + index as u64,
                cursor_token: (index + 2).to_string().repeat(32),
                projection,
            })
            .collect::<Vec<_>>();
        let actual = json!({
            "state": replica_state_json(&ReplicaState {
                server_id: "0123456789abcdef0123456789abcdef".into(),
                cursor: 7,
                cursor_token: "11111111111111111111111111111111".into(),
                next_review_sequence: 3,
            }).unwrap(),
            "snapshot_partial": replica_snapshot_json(&page).unwrap(),
            "snapshot_terminal": replica_snapshot_json(&terminal).unwrap(),
            "changes": replica_changes_json(
                "0123456789abcdef0123456789abcdef",
                7,
                "11111111111111111111111111111111",
                10,
                &changes,
            ).unwrap(),
        });
        let expected: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/protocol/replica-sync-v1.json"
        ))
        .unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn one_projection_cannot_exceed_the_response_budget() {
        let mut projection = document_projection();
        let ReplicaProjection::Document(document) = &mut projection else {
            unreachable!();
        };
        document.projection.as_mut().unwrap().title = "x".repeat(super::MAX_REPLICA_PAGE_BYTES);
        let page = SnapshotPage {
            position: SnapshotPosition {
                snapshot_id: "snapshot".into(),
                server_id: "server".into(),
                cut_cursor: 0,
                cut_token: "0".repeat(32),
                after: 0,
            },
            items: vec![projection],
            next: None,
        };

        assert!(matches!(
            replica_snapshot_json(&page),
            Err(ReplicaWireError::PageTooLarge)
        ));
    }

    fn document_projection() -> ReplicaProjection {
        ReplicaProjection::Document(DocumentReplicaRevision {
            revision: DocumentRevision {
                document_id: "document".into(),
                revision: 2,
                logical_path: "note.org".into(),
                blob_sha256: Some("a".repeat(64)),
                operation: "update".into(),
                created_at_us: 20,
                author_device_id: Some("device".into()),
            },
            projection: Some(DocumentProjectionSnapshot {
                title: "Note".into(),
                file_node_id: Some("node".into()),
                file_tags: vec!["tag".into()],
                nodes: vec![ProjectionNode {
                    node_id: "node".into(),
                    title: "Heading".into(),
                    title_path: vec!["Heading".into()],
                    tags: vec!["tag".into()],
                    parent_id: None,
                    level: 1,
                }],
                link_occurrences: vec![ProjectionLinkOccurrence {
                    owner_id: "node".into(),
                    target_id: "target".into(),
                    start: 10,
                    end: 23,
                }],
                semantic_edges: vec![ProjectionEdge {
                    source_id: "node".into(),
                    target_id: "target".into(),
                }],
                compatibility_sha1: "b".repeat(40),
                content_sha256: "a".repeat(64),
            }),
        })
    }

    fn tombstone_projection() -> ReplicaProjection {
        ReplicaProjection::Asset(AssetRevision {
            logical_path: format!("assets/{}/image.png", "c".repeat(64)),
            revision: 2,
            blob_sha256: None,
            size_bytes: None,
            media_type: None,
            operation: "tombstone".into(),
            created_at_us: 21,
            author_device_id: Some("device".into()),
        })
    }

    fn supplemental_projections() -> Vec<ReplicaProjection> {
        vec![
            thema_projection(),
            settings_projection(),
            review_projection(),
        ]
    }

    fn thema_projection() -> ReplicaProjection {
        ReplicaProjection::Thema(ThemaRevision {
            record: ThemaRecord {
                thema_id: "thema".into(),
                revision: 3,
                content: thema_content(
                    "basic",
                    "Question".into(),
                    vec![None],
                    vec!["Answer".into()],
                    Some("source".into()),
                    None,
                    None,
                )
                .unwrap(),
                content_sha256: "d".repeat(64),
                tags: vec!["tag".into()],
                node_ids: vec!["node".into()],
                asset_paths: vec![],
                archived: false,
            },
            operation: "update".into(),
            created_at_us: 22,
            author_device_id: Some("device".into()),
        })
    }

    fn settings_projection() -> ReplicaProjection {
        ReplicaProjection::Settings(SettingsRevision {
            settings: SettingsRecord {
                revision: 2,
                timezone: "UTC".into(),
                day_boundary_hour: 4,
                scheduler: ParametersSnapshot {
                    scheduler_version: 1,
                    amnesia: 0.5,
                    epignosis: 0.1,
                    agnoia: 0.2,
                    anagnosis: 3,
                    lethe: 2,
                    synolon_max: 3.0,
                    interval_fuzz: 0.1,
                    proto: vec![0, 1, 2],
                },
                tag_rules: BTreeMap::from([(
                    "tag".into(),
                    TagRule {
                        amnesia: Some(0.6),
                        ..TagRule::default()
                    },
                )]),
            },
            operation: "update".into(),
            created_at_us: 23,
            author_device_id: Some("device".into()),
        })
    }

    fn review_projection() -> ReplicaProjection {
        ReplicaProjection::ReviewState(ReviewStateRevision {
            thema_id: "thema".into(),
            state: ReviewStateSnapshot {
                state_revision: 1,
                review_count: 1,
                legacy_review_count: 0,
                legacy_unclassified_count: 0,
                last_reviewed_at_us: Some(24),
                last_review_day: Some(20260715),
                next_review_day: 20260716,
                interval_days: Some(1),
                consecutive_successes: 1,
                total_successes: 1,
                consecutive_failures: 0,
                total_failures: 0,
                suspended: false,
                gnosis_plus: 0.4,
                gnosis_minus: 0.3,
                gnosis_synolon: 1.4,
            },
            event_id: Some("event".into()),
            created_at_us: 24,
        })
    }
}
