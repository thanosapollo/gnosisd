use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::application::{
    DocumentProjectionSnapshot, ParametersSnapshot, ProjectionEdge, ProjectionLinkOccurrence,
    ProjectionNode, ReplicaChange, ReplicaProjection, ReplicaState, ReviewStateSnapshot,
    SettingsInput, SnapshotPage, SnapshotPosition, TagRule, prepare_thema, thema_content,
    validate_asset_path, validate_org_path,
};
use crate::is_canonical_sha256;

use super::http_v1::{
    AssetOperationV1, AssetRevisionV1, DocumentOperationV1, DocumentProjectionV1,
    DocumentRevisionV1, ParametersV1, ProjectionEdgeV1, ProjectionLinkV1, ProjectionNodeV1,
    ReplicaChangePageV1, ReplicaChangeV1, ReplicaProjectionV1, ReplicaSnapshotPageV1,
    ReplicaStateV1, ReviewStateRevisionV1, ReviewStateV1, SettingsOperationV1, SettingsRevisionV1,
    TagRuleV1, ThemaKindV1, ThemaOperationV1, ThemaRevisionV1,
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
    #[error("replica response is invalid")]
    Response,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DecodedReplicaState {
    pub(crate) server_id: String,
    pub(crate) cursor: String,
    pub(crate) next_device_sequence: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DecodedReplicaItem {
    pub(crate) entity_kind: String,
    pub(crate) entity_id: String,
    pub(crate) revision: u64,
    pub(crate) tombstone: bool,
    pub(crate) blob_digests: Vec<String>,
    pub(crate) payload_json: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DecodedChangePage {
    pub(crate) server_id: String,
    pub(crate) from: String,
    pub(crate) next: String,
    pub(crate) has_more: bool,
    pub(crate) items: Vec<DecodedReplicaItem>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DecodedSnapshotPage {
    pub(crate) snapshot_id: String,
    pub(crate) server_id: String,
    pub(crate) cut_cursor: u64,
    pub(crate) cut_token: String,
    pub(crate) cursor: Option<String>,
    pub(crate) next: Option<String>,
    pub(crate) next_after: Option<u64>,
    pub(crate) items: Vec<DecodedReplicaItem>,
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

pub(crate) fn replica_state_response(
    state: &ReplicaState,
) -> Result<ReplicaStateV1, ReplicaWireError> {
    Ok(ReplicaStateV1 {
        version: REPLICA_VERSION,
        server_id: state.server_id.clone(),
        cursor: encode_cursor(&state.server_id, state.cursor, &state.cursor_token)?,
        next_device_sequence: state.next_device_sequence,
        snapshot_required: true,
    })
}

pub(crate) fn replica_change_page(
    server_id: &str,
    from: u64,
    from_token: &str,
    latest: u64,
    changes: &[ReplicaChange],
) -> Result<ReplicaChangePageV1, ReplicaWireError> {
    let items = changes
        .iter()
        .map(|change| ReplicaChangeV1::from_change(server_id, change))
        .collect::<Result<Vec<_>, _>>()?;
    encode_change_page(server_id, from, from_token, latest, changes, &items)
}

pub(crate) fn replica_snapshot_page(
    page: &SnapshotPage,
) -> Result<ReplicaSnapshotPageV1, ReplicaWireError> {
    let items = page
        .items
        .iter()
        .map(ReplicaProjectionV1::try_from)
        .collect::<Result<Vec<_>, _>>()?;
    encode_snapshot_page(page, &items)
}

pub(crate) fn decode_replica_state(
    state: ReplicaStateV1,
) -> Result<DecodedReplicaState, ReplicaWireError> {
    require_response_size(&state)?;
    if state.version != REPLICA_VERSION || !state.snapshot_required {
        return Err(ReplicaWireError::Response);
    }
    let (server_id, _, _) = decode_cursor(&state.cursor)?;
    if server_id != state.server_id || state.server_id.is_empty() {
        return Err(ReplicaWireError::Response);
    }
    Ok(DecodedReplicaState {
        server_id: state.server_id,
        cursor: state.cursor,
        next_device_sequence: state.next_device_sequence,
    })
}

pub(crate) fn decode_change_page(
    page: ReplicaChangePageV1,
) -> Result<DecodedChangePage, ReplicaWireError> {
    require_response_size(&page)?;
    if page.version != REPLICA_VERSION {
        return Err(ReplicaWireError::Response);
    }
    let (server_id, from, _) = decode_cursor(&page.from)?;
    let (next_server, next, _) = decode_cursor(&page.next)?;
    let terminal_matches = page
        .items
        .last()
        .map_or(page.from == page.next, |item| item.cursor == page.next);
    if server_id != next_server
        || next < from
        || !terminal_matches
        || (page.has_more && page.items.is_empty())
    {
        return Err(ReplicaWireError::Response);
    }
    let items = decode_changes(&page.items, &server_id, from, next)?;
    Ok(DecodedChangePage {
        server_id,
        from: page.from,
        next: page.next,
        has_more: page.has_more,
        items,
    })
}

pub(crate) fn decode_snapshot_page(
    page: ReplicaSnapshotPageV1,
) -> Result<DecodedSnapshotPage, ReplicaWireError> {
    require_response_size(&page)?;
    if page.version != REPLICA_VERSION || page.snapshot_id.is_empty() {
        return Err(ReplicaWireError::Response);
    }
    let identity = snapshot_identity(&page)?;
    let items = page
        .items
        .iter()
        .map(DecodedReplicaItem::try_from)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(DecodedSnapshotPage {
        snapshot_id: page.snapshot_id,
        server_id: identity.0,
        cut_cursor: identity.1,
        cut_token: identity.2,
        cursor: page.cursor,
        next: page.next,
        next_after: identity.3,
        items,
    })
}

fn decode_changes(
    changes: &[ReplicaChangeV1],
    server_id: &str,
    from: u64,
    next: u64,
) -> Result<Vec<DecodedReplicaItem>, ReplicaWireError> {
    let mut previous = from;
    let mut decoded = Vec::with_capacity(changes.len());
    for change in changes {
        let (change_server, position, _) = decode_cursor(&change.cursor)?;
        if change_server != server_id || position <= previous || position > next {
            return Err(ReplicaWireError::Response);
        }
        previous = position;
        decoded.push(DecodedReplicaItem::try_from(&change.projection)?);
    }
    if previous != next {
        return Err(ReplicaWireError::Response);
    }
    Ok(decoded)
}

fn snapshot_identity(
    page: &ReplicaSnapshotPageV1,
) -> Result<(String, u64, String, Option<u64>), ReplicaWireError> {
    match (&page.cursor, &page.next) {
        (Some(cursor), None) => {
            let (server_id, cut, token) = decode_cursor(cursor)?;
            Ok((server_id, cut, token, None))
        }
        (None, Some(next)) => {
            let position = decode_snapshot_cursor(next)?;
            if position.snapshot_id != page.snapshot_id || position.after == 0 {
                return Err(ReplicaWireError::Response);
            }
            Ok((
                position.server_id,
                position.cut_cursor,
                position.cut_token,
                Some(position.after),
            ))
        }
        _ => Err(ReplicaWireError::Response),
    }
}

fn require_response_size(value: &impl Serialize) -> Result<(), ReplicaWireError> {
    let bytes = serde_json::to_vec(value).map_err(|_| ReplicaWireError::Response)?;
    if bytes.len() <= MAX_REPLICA_PAGE_BYTES {
        Ok(())
    } else {
        Err(ReplicaWireError::Response)
    }
}

fn encode_change_page(
    server_id: &str,
    from: u64,
    from_token: &str,
    latest: u64,
    changes: &[ReplicaChange],
    items: &[ReplicaChangeV1],
) -> Result<ReplicaChangePageV1, ReplicaWireError> {
    for count in (0..=items.len()).rev() {
        let next_change = count.checked_sub(1).and_then(|index| changes.get(index));
        let next = next_change.map_or(from, |change| change.cursor);
        let token = next_change.map_or(from_token, |change| change.cursor_token.as_str());
        let page = ReplicaChangePageV1 {
            version: REPLICA_VERSION,
            from: encode_cursor(server_id, from, from_token)?,
            next: encode_cursor(server_id, next, token)?,
            has_more: count < items.len() || next < latest,
            items: items[..count].to_vec(),
        };
        if let Some(page) = bounded(page)? {
            if count == 0 && !items.is_empty() {
                return Err(ReplicaWireError::PageTooLarge);
            }
            return Ok(page);
        }
    }
    Err(ReplicaWireError::PageTooLarge)
}

fn encode_snapshot_page(
    page: &SnapshotPage,
    items: &[ReplicaProjectionV1],
) -> Result<ReplicaSnapshotPageV1, ReplicaWireError> {
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
        let wire = ReplicaSnapshotPageV1 {
            version: REPLICA_VERSION,
            snapshot_id: page.position.snapshot_id.clone(),
            cursor,
            next,
            items: items[..count].to_vec(),
        };
        if let Some(wire) = bounded(wire)? {
            if count == 0 && !items.is_empty() {
                return Err(ReplicaWireError::PageTooLarge);
            }
            return Ok(wire);
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

fn bounded<T: Serialize>(value: T) -> Result<Option<T>, ReplicaWireError> {
    let bytes = serde_json::to_vec(&value).map_err(|_| ReplicaWireError::Encode)?;
    if bytes.len() > MAX_REPLICA_PAGE_BYTES {
        return Ok(None);
    }
    Ok(Some(value))
}

impl ReplicaChangeV1 {
    fn from_change(server_id: &str, change: &ReplicaChange) -> Result<Self, ReplicaWireError> {
        Ok(Self {
            cursor: encode_cursor(server_id, change.cursor, &change.cursor_token)?,
            projection: ReplicaProjectionV1::try_from(&change.projection)?,
        })
    }
}

impl TryFrom<&ReplicaProjection> for ReplicaProjectionV1 {
    type Error = ReplicaWireError;

    fn try_from(value: &ReplicaProjection) -> Result<Self, Self::Error> {
        Ok(match value {
            ReplicaProjection::Document(value) => Self::Document(DocumentRevisionV1 {
                document_id: value.revision.document_id.clone(),
                revision: value.revision.revision,
                logical_path: value.revision.logical_path.clone(),
                content_sha256: value.revision.blob_sha256.clone(),
                blob_url: value.revision.blob_sha256.as_deref().map(blob_url),
                operation: document_operation(&value.revision.operation)?,
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
                operation: asset_operation(&value.operation)?,
                created_at_us: value.created_at_us,
                author_device_id: value.author_device_id.clone(),
            }),
            ReplicaProjection::Thema(value) => Self::Thema(ThemaRevisionV1::try_from(value)?),
            ReplicaProjection::Settings(value) => {
                Self::Settings(SettingsRevisionV1::try_from(value)?)
            }
            ReplicaProjection::ReviewState(value) => Self::ReviewState(ReviewStateRevisionV1 {
                thema_id: value.thema_id.clone(),
                state: ReviewStateV1::from(&value.state),
                event_id: value.event_id.clone(),
                created_at_us: value.created_at_us,
            }),
        })
    }
}

impl TryFrom<&ReplicaProjectionV1> for DecodedReplicaItem {
    type Error = ReplicaWireError;

    fn try_from(value: &ReplicaProjectionV1) -> Result<Self, Self::Error> {
        let (kind, id, revision, tombstone, blobs) = projection_identity(value)?;
        let payload_json = serde_json::to_string(value).map_err(|_| ReplicaWireError::Response)?;
        Ok(Self {
            entity_kind: kind.into(),
            entity_id: id,
            revision,
            tombstone,
            blob_digests: blobs,
            payload_json,
        })
    }
}

type ProjectionIdentity = (&'static str, String, u64, bool, Vec<String>);

fn projection_identity(
    value: &ReplicaProjectionV1,
) -> Result<ProjectionIdentity, ReplicaWireError> {
    match value {
        ReplicaProjectionV1::Document(value) => document_identity(value),
        ReplicaProjectionV1::Asset(value) => asset_identity(value),
        ReplicaProjectionV1::Thema(value) => thema_identity(value),
        ReplicaProjectionV1::Settings(value) => settings_identity(value),
        ReplicaProjectionV1::ReviewState(value) => review_identity(value),
    }
}

fn document_identity(value: &DocumentRevisionV1) -> Result<ProjectionIdentity, ReplicaWireError> {
    require_identity(&value.document_id)?;
    validate_org_path(&value.logical_path).map_err(|_| ReplicaWireError::Response)?;
    validate_blob_reference(value.content_sha256.as_deref(), value.blob_url.as_deref())?;
    let tombstone = value.operation == DocumentOperationV1::Tombstone;
    let invalid_tombstone =
        tombstone && (value.projection.is_some() || value.content_sha256.is_some());
    let invalid_active = !tombstone
        && (value.projection.is_none()
            || (value.operation == DocumentOperationV1::Snapshot)
                != value.content_sha256.is_none());
    if invalid_tombstone || invalid_active {
        return Err(ReplicaWireError::Response);
    }
    if let Some(projection) = &value.projection {
        require_digest(&projection.content_sha256)?;
        if value
            .content_sha256
            .as_ref()
            .is_some_and(|digest| digest != &projection.content_sha256)
        {
            return Err(ReplicaWireError::Response);
        }
    }
    Ok((
        "document",
        value.document_id.clone(),
        value.revision,
        tombstone,
        value.content_sha256.iter().cloned().collect(),
    ))
}

fn asset_identity(value: &AssetRevisionV1) -> Result<ProjectionIdentity, ReplicaWireError> {
    validate_asset_path(&value.logical_path).map_err(|_| ReplicaWireError::Response)?;
    validate_blob_reference(value.content_sha256.as_deref(), value.blob_url.as_deref())?;
    let tombstone = value.operation == AssetOperationV1::Tombstone;
    let has_content = value.content_sha256.is_some()
        && value.size_bytes.is_some()
        && value
            .media_type
            .as_ref()
            .is_some_and(|value| !value.is_empty());
    if tombstone == has_content {
        return Err(ReplicaWireError::Response);
    }
    if value
        .content_sha256
        .as_ref()
        .is_some_and(|digest| value.logical_path.split('/').nth(1) != Some(digest.as_str()))
    {
        return Err(ReplicaWireError::Response);
    }
    Ok((
        "asset",
        value.logical_path.clone(),
        value.revision,
        tombstone,
        value.content_sha256.iter().cloned().collect(),
    ))
}

fn thema_identity(value: &ThemaRevisionV1) -> Result<ProjectionIdentity, ReplicaWireError> {
    require_identity(&value.thema_id)?;
    require_digest(&value.content_sha256)?;
    let content = thema_content(
        value.kind.as_str(),
        value.keimenon.clone(),
        value.hypothesis.clone(),
        value.answer.clone(),
        value.source_guid.clone(),
        value.parathema.clone(),
        value.review_image.clone(),
    )
    .map_err(|_| ReplicaWireError::Response)?;
    let prepared = prepare_thema(
        Some(value.thema_id.clone()),
        content,
        value.tags.clone(),
        value.node_ids.clone(),
        value.asset_paths.clone(),
        None,
    )
    .map_err(|_| ReplicaWireError::Response)?;
    if prepared.content_sha256 != value.content_sha256 {
        return Err(ReplicaWireError::Response);
    }
    Ok((
        "thema",
        value.thema_id.clone(),
        value.revision,
        false,
        Vec::new(),
    ))
}

fn parameters_snapshot(value: &ParametersV1) -> ParametersSnapshot {
    ParametersSnapshot {
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

fn settings_identity(value: &SettingsRevisionV1) -> Result<ProjectionIdentity, ReplicaWireError> {
    if value.timezone.is_empty() || value.day_boundary_hour > 23 {
        return Err(ReplicaWireError::Response);
    }
    let settings = SettingsInput {
        timezone: value.timezone.clone(),
        day_boundary_hour: value.day_boundary_hour,
        scheduler: parameters_snapshot(&value.scheduler),
        tag_rules: value
            .tag_rules
            .iter()
            .map(|(tag, rule)| {
                (
                    tag.clone(),
                    TagRule {
                        amnesia: rule.amnesia,
                        epignosis: rule.epignosis,
                        agnoia: rule.agnoia,
                        anagnosis: rule.anagnosis,
                        lethe: rule.lethe,
                        proto: rule.proto.clone(),
                    },
                )
            })
            .collect(),
    };
    settings
        .validate()
        .map_err(|_| ReplicaWireError::Response)?;
    Ok((
        "settings",
        "settings".into(),
        value.revision,
        false,
        Vec::new(),
    ))
}

fn review_identity(value: &ReviewStateRevisionV1) -> Result<ProjectionIdentity, ReplicaWireError> {
    require_identity(&value.thema_id)?;
    if (value.state.state_revision == 0) != value.event_id.is_none()
        || value.event_id.as_ref().is_some_and(String::is_empty)
        || value.created_at_us == 0
    {
        return Err(ReplicaWireError::Response);
    }
    let state = ReviewStateSnapshot {
        state_revision: value.state.state_revision,
        review_count: value.state.review_count,
        legacy_review_count: value.state.legacy_review_count,
        legacy_unclassified_count: value.state.legacy_unclassified_count,
        last_reviewed_at_us: value.state.last_reviewed_at_us,
        last_review_day: value.state.last_review_day,
        next_review_day: value.state.next_review_day,
        interval_days: value.state.interval_days,
        consecutive_successes: value.state.consecutive_successes,
        total_successes: value.state.total_successes,
        consecutive_failures: value.state.consecutive_failures,
        total_failures: value.state.total_failures,
        suspended: value.state.suspended,
        gnosis_plus: value.state.gnosis_plus,
        gnosis_minus: value.state.gnosis_minus,
        gnosis_synolon: value.state.gnosis_synolon,
    };
    crate::domain::validate_state(&state.into()).map_err(|_| ReplicaWireError::Response)?;
    Ok((
        "review",
        value.thema_id.clone(),
        value.state.state_revision,
        false,
        Vec::new(),
    ))
}

fn require_identity(value: &str) -> Result<(), ReplicaWireError> {
    if value.is_empty() || value.len() > 4096 {
        Err(ReplicaWireError::Response)
    } else {
        Ok(())
    }
}

fn require_digest(value: &str) -> Result<(), ReplicaWireError> {
    if is_canonical_sha256(value) {
        Ok(())
    } else {
        Err(ReplicaWireError::Response)
    }
}

fn validate_blob_reference(
    digest: Option<&str>,
    url: Option<&str>,
) -> Result<(), ReplicaWireError> {
    match (digest, url) {
        (Some(digest), Some(url)) if url == blob_url(digest) => require_digest(digest),
        (None, None) => Ok(()),
        _ => Err(ReplicaWireError::Response),
    }
}

fn document_operation(value: &str) -> Result<DocumentOperationV1, ReplicaWireError> {
    match value {
        "create" => Ok(DocumentOperationV1::Create),
        "update" => Ok(DocumentOperationV1::Update),
        "rename" => Ok(DocumentOperationV1::Rename),
        "snapshot" => Ok(DocumentOperationV1::Snapshot),
        "tombstone" => Ok(DocumentOperationV1::Tombstone),
        _ => Err(ReplicaWireError::Response),
    }
}

fn asset_operation(value: &str) -> Result<AssetOperationV1, ReplicaWireError> {
    match value {
        "create" => Ok(AssetOperationV1::Create),
        "update" => Ok(AssetOperationV1::Update),
        "tombstone" => Ok(AssetOperationV1::Tombstone),
        _ => Err(ReplicaWireError::Response),
    }
}

fn thema_kind(value: &str) -> Result<ThemaKindV1, ReplicaWireError> {
    match value {
        "basic" => Ok(ThemaKindV1::Basic),
        "cloze" => Ok(ThemaKindV1::Cloze),
        "mcq" => Ok(ThemaKindV1::Mcq),
        "mc-cloze" => Ok(ThemaKindV1::McCloze),
        _ => Err(ReplicaWireError::Response),
    }
}

fn thema_operation(value: &str) -> Result<ThemaOperationV1, ReplicaWireError> {
    match value {
        "create" => Ok(ThemaOperationV1::Create),
        "update" => Ok(ThemaOperationV1::Update),
        "archive" => Ok(ThemaOperationV1::Archive),
        "unarchive" => Ok(ThemaOperationV1::Unarchive),
        "snapshot" => Ok(ThemaOperationV1::Snapshot),
        _ => Err(ReplicaWireError::Response),
    }
}

fn settings_operation(value: &str) -> Result<SettingsOperationV1, ReplicaWireError> {
    match value {
        "snapshot" => Ok(SettingsOperationV1::Snapshot),
        "update" => Ok(SettingsOperationV1::Update),
        _ => Err(ReplicaWireError::Response),
    }
}

fn blob_url(digest: &str) -> String {
    format!("/v1/blobs/{digest}")
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

impl From<&ProjectionEdge> for ProjectionEdgeV1 {
    fn from(value: &ProjectionEdge) -> Self {
        Self {
            source_id: value.source_id.clone(),
            target_id: value.target_id.clone(),
        }
    }
}

impl TryFrom<&crate::application::ThemaRevision> for ThemaRevisionV1 {
    type Error = ReplicaWireError;

    fn try_from(value: &crate::application::ThemaRevision) -> Result<Self, Self::Error> {
        Ok(Self {
            thema_id: value.record.thema_id.clone(),
            revision: value.record.revision,
            kind: thema_kind(value.record.content.kind.as_str())?,
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
            operation: thema_operation(&value.operation)?,
            created_at_us: value.created_at_us,
            author_device_id: value.author_device_id.clone(),
        })
    }
}

impl TryFrom<&crate::application::SettingsRevision> for SettingsRevisionV1 {
    type Error = ReplicaWireError;

    fn try_from(value: &crate::application::SettingsRevision) -> Result<Self, Self::Error> {
        Ok(Self {
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
            operation: settings_operation(&value.operation)?,
            created_at_us: value.created_at_us,
            author_device_id: value.author_device_id.clone(),
        })
    }
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
        CursorV1, MAX_REPLICA_CURSOR_BYTES, ReplicaWireError, decode_cursor, decode_replica_state,
        decode_snapshot_cursor, encode_cursor, encode_snapshot_cursor, encode_token,
        replica_change_page, replica_snapshot_page, replica_state_response,
    };
    use crate::application::{
        AssetRevision, DocumentProjectionSnapshot, DocumentReplicaRevision, DocumentRevision,
        ParametersSnapshot, ProjectionEdge, ProjectionLinkOccurrence, ProjectionNode,
        ReplicaChange, ReplicaProjection, ReplicaState, ReviewStateRevision, ReviewStateSnapshot,
        SettingsRecord, SettingsRevision, SnapshotPage, SnapshotPosition, TagRule, ThemaRecord,
        ThemaRevision, prepare_thema, thema_content,
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
            "state": replica_state_response(&ReplicaState {
                server_id: "0123456789abcdef0123456789abcdef".into(),
                cursor: 7,
                cursor_token: "11111111111111111111111111111111".into(),
                next_device_sequence: 3,
            }).unwrap(),
            "snapshot_partial": replica_snapshot_page(&page).unwrap(),
            "snapshot_terminal": replica_snapshot_page(&terminal).unwrap(),
            "changes": replica_change_page(
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
    fn legacy_sequence_name_is_accepted_but_dual_names_must_agree() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/protocol/replica-sync-v1.json"
        ))
        .unwrap();
        let legacy = serde_json::from_value(fixture["state"].clone()).unwrap();
        assert_eq!(
            decode_replica_state(legacy).unwrap().next_device_sequence,
            3
        );

        let mut inconsistent = fixture["state"].clone();
        inconsistent["next_device_sequence"] = json!(4);
        assert!(
            serde_json::from_value::<crate::protocol::http_v1::ReplicaStateV1>(inconsistent)
                .is_err()
        );
    }

    #[test]
    fn current_v1_state_is_accepted_by_the_frozen_legacy_decoder() {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct LegacyState {
            version: u32,
            server_id: String,
            cursor: String,
            next_review_sequence: u64,
            snapshot_required: bool,
        }
        let wire = replica_state_response(&ReplicaState {
            server_id: "0".repeat(32),
            cursor: 0,
            cursor_token: "1".repeat(32),
            next_device_sequence: 3,
        })
        .unwrap();

        let legacy: LegacyState =
            serde_json::from_value(serde_json::to_value(wire).unwrap()).unwrap();

        assert_eq!(legacy.version, 1);
        assert_eq!(legacy.server_id, "0".repeat(32));
        assert!(!legacy.cursor.is_empty());
        assert_eq!(legacy.next_review_sequence, 3);
        assert!(legacy.snapshot_required);
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
            replica_snapshot_page(&page),
            Err(ReplicaWireError::PageTooLarge)
        ));
    }

    #[test]
    fn mc_cloze_projection_round_trips_through_v1() {
        let content = thema_content(
            "mc-cloze",
            "Question".into(),
            vec![Some("First".into()), Some("Second".into())],
            vec!["First".into()],
            None,
            None,
            None,
        )
        .unwrap();
        let digest = prepare_thema(
            Some("thema".into()),
            content.clone(),
            vec![],
            vec![],
            vec![],
            None,
        )
        .unwrap()
        .content_sha256;
        let page = SnapshotPage {
            position: SnapshotPosition {
                snapshot_id: "snapshot".into(),
                server_id: "0".repeat(32),
                cut_cursor: 1,
                cut_token: "1".repeat(32),
                after: 0,
            },
            items: vec![ReplicaProjection::Thema(ThemaRevision {
                record: ThemaRecord {
                    thema_id: "thema".into(),
                    revision: 1,
                    content,
                    content_sha256: digest,
                    tags: vec![],
                    node_ids: vec![],
                    asset_paths: vec![],
                    archived: false,
                },
                operation: "create".into(),
                created_at_us: 1,
                author_device_id: Some("device".into()),
            })],
            next: None,
        };

        let wire = replica_snapshot_page(&page).unwrap();
        assert_eq!(
            serde_json::to_value(&wire).unwrap()["items"][0]["projection"]["kind"],
            "mc-cloze"
        );
        assert_eq!(super::decode_snapshot_page(wire).unwrap().items.len(), 1);
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
