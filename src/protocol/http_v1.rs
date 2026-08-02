//! Public JSON contracts for authenticated HTTP `/v1` clients.
//!
//! These DTOs preserve their exact versioned JSON shapes. Offline-capable
//! clients validate cursor, identity, projection, and ordering before local
//! persistence.

mod client_views;
mod content;

pub use super::offline_asset::{
    OfflineAssetBaseV1, OfflineAssetConflictReasonV1, OfflineAssetMutationKindV1,
    OfflineAssetOperationEnvelopeV1, OfflineAssetOperationV1, OfflineAssetProjectionV1,
    OfflineAssetResultEnvelopeV1, OfflineAssetResultV1,
};
pub use super::offline_content::{
    OfflineContentBaseV1, OfflineContentConflictReasonV1, OfflineContentDocumentV1,
    OfflineContentMutationV1, OfflineContentOperationEnvelopeV1, OfflineContentOperationV1,
    OfflineContentResultEnvelopeV1, OfflineContentResultV1,
};
pub use super::offline_review::{
    ActiveReviewPackEnvelopeV2, OfflineAnswerV1, OfflineFsrsConfigV1, OfflinePresentationV1,
    OfflineReviewConflictReasonV2, OfflineReviewOperationEnvelopeV1, OfflineReviewOperationV1,
    OfflineReviewResultEnvelopeV2, OfflineReviewResultV2, OfflineReviewSessionModeV2,
    OfflineReviewSessionStatusV2, OfflineReviewSessionV2, OfflineReviewSourceKindV2,
    OfflineReviewSourceV2, OfflineReviewStateV1, OfflineThemaKindV1, OfflineTransitionV1,
    ReviewPackAssetV2, ReviewPackEnvelopeV2, ReviewPackItemV2, ReviewPackStatusV2, ReviewPackV2,
};
pub use client_views::{
    ContentCountsV1, DashboardV1, DocumentView, DuePageView, DueThemaView, LinkOccurrenceView,
    NodeEdgeView, NodeView, ReviewClozeCheckRequestV1, ReviewClozeCheckV1, ReviewHistoryEventV1,
    ReviewHistoryPageV1, ReviewHistoryRequestV1, ReviewQueueCountsV1, ReviewQueueSummaryRequestV1,
    ReviewQueueSummaryV1, ReviewSessionListRequestV1, ReviewSessionPageV1, ReviewSessionSummaryV1,
    ReviewTotalsV1, ReviewWindowV1, ThemaReviewStateV1,
};
pub use content::{
    AssetHeadV1, ContentChangePageV1, ContentChangeV1, ContentEntityKindV1, ContentOperationV1,
    ContentSearchHitV1, ContentSearchPageV1, DesiredStateMutationProjectionV1,
    DesiredStateMutationResultV1, DocumentHeadV1, DocumentImportRequestV1,
    DocumentMutationRejectionV1, DocumentMutationResultV1, DocumentRevisionPageV1,
    DocumentRevisionSummaryV1, NodeCatalogItemV1, NodeCatalogPageV1, NodePageV1,
    NodeThemaAssociationPageV1, NodeThemaAssociationV1, NodeV1, NoteRenameRequestV1,
    NoteWriteRequestV1, SettingsV1, StagedBlobV1, ThemaCatalogItemV1, ThemaCatalogPageV1,
    ThemaConceptRelationV1, ThemaConceptV1, ThemaPageV1, ThemaV1, ThemaWriteRequestV1,
    UpdateSettingsRequestV1,
};

use std::borrow::Cow;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize, de};
use sha2::{Digest, Sha256};

use crate::application::{
    AnswerInput, ContentSearchGeneration, FsrsAlgorithmSnapshot, FsrsConfigSnapshot,
    FsrsImplementationSnapshot, FsrsModelSnapshot, PresentationSnapshot, RecallOutcomeSnapshot,
    ReviewAttemptBeginResult, ReviewAttemptOperationResult, ReviewAttemptState, ReviewAttemptView,
    ReviewClozeSpan, ReviewClozeState, ReviewItemSkipReason, ReviewQuestion, ReviewResponsePrompt,
    ReviewSessionMode, ReviewSessionView, ReviewSource, ReviewSourceKind, SkippedReviewItem,
    TransitionSnapshot, review_attempt_question,
};

const MIN_BEARER_TOKEN_BYTES: usize = 40;
const MAX_BEARER_TOKEN_BYTES: usize = 128;
pub(crate) const MAX_CONTENT_SEARCH_CURSOR_BYTES: usize = 2 * 1024;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ContentSearchCursorV1 {
    version: u32,
    server_id: String,
    generation_cursor: u64,
    generation_token: String,
    query_digest: String,
    next_document_id: String,
}

pub(crate) struct ContentSearchPosition {
    pub(crate) generation: ContentSearchGeneration,
    pub(crate) after: String,
}

pub(crate) fn normalize_content_search_query(value: &str) -> Result<String, ()> {
    let value = value.trim();
    if (1..=256).contains(&value.chars().count()) {
        Ok(value.to_owned())
    } else {
        Err(())
    }
}

pub(crate) fn encode_content_search_cursor(
    generation: &ContentSearchGeneration,
    query: &str,
    next_document_id: &str,
) -> Result<String, ()> {
    let cursor = ContentSearchCursorV1 {
        version: 1,
        server_id: generation.server_id.clone(),
        generation_cursor: generation.cursor,
        generation_token: generation.cursor_token.clone(),
        query_digest: format!("{:x}", Sha256::digest(query.as_bytes())),
        next_document_id: next_document_id.to_owned(),
    };
    let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&cursor).map_err(|_| ())?);
    if encoded.len() <= MAX_CONTENT_SEARCH_CURSOR_BYTES {
        Ok(encoded)
    } else {
        Err(())
    }
}

pub(crate) fn decode_content_search_cursor(
    value: &str,
    query: &str,
) -> Result<ContentSearchPosition, ()> {
    if value.len() > MAX_CONTENT_SEARCH_CURSOR_BYTES {
        return Err(());
    }
    let bytes = URL_SAFE_NO_PAD.decode(value).map_err(|_| ())?;
    let cursor: ContentSearchCursorV1 = serde_json::from_slice(&bytes).map_err(|_| ())?;
    if cursor.version != 1
        || cursor.query_digest != format!("{:x}", Sha256::digest(query.as_bytes()))
        || cursor.server_id.is_empty()
        || cursor.generation_token.is_empty()
        || cursor.next_document_id.is_empty()
    {
        return Err(());
    }
    Ok(ContentSearchPosition {
        generation: ContentSearchGeneration {
            server_id: cursor.server_id,
            cursor: cursor.generation_cursor,
            cursor_token: cursor.generation_token,
        },
        after: cursor.next_document_id,
    })
}

pub(crate) fn valid_bearer_token(value: &str) -> bool {
    (MIN_BEARER_TOKEN_BYTES..=MAX_BEARER_TOKEN_BYTES).contains(&value.len())
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'+' | b'/' | b'=')
        })
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
/// The device identity bound to the authenticated bearer token.
pub struct DeviceIdentityV1 {
    #[schemars(extend("const" = 1))]
    pub version: u32,
    #[schemars(length(min = 1, max = 128))]
    pub device_id: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// Server identity, durable cursor, and next device sequence used to bootstrap a replica.
pub struct ReplicaStateV1 {
    #[schemars(extend("const" = 1))]
    pub version: u32,
    #[schemars(regex(pattern = "^[0-9a-f]{32}$"))]
    pub server_id: String,
    #[schemars(length(min = 1, max = 512))]
    pub cursor: String,
    pub next_device_sequence: u64,
    #[schemars(extend("const" = true))]
    pub snapshot_required: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContentCapabilitiesV1 {
    #[schemars(extend("const" = 1))]
    pub version: u32,
    #[schemars(regex(pattern = "^[0-9a-f]{32}$"))]
    pub server_id: String,
    #[schemars(extend("const" = 1))]
    pub document_lifecycle: u32,
    #[schemars(length(min = 1, max = 16))]
    pub supported_content_types: Vec<String>,
    #[schemars(extend("const" = 1048576))]
    pub max_source_bytes: u64,
    #[schemars(extend("const" = 1048576))]
    pub max_projection_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AssetCapabilitiesV1 {
    #[schemars(extend("const" = 1))]
    pub version: u32,
    #[schemars(regex(pattern = "^[0-9a-f]{32}$"))]
    pub server_id: String,
    #[schemars(extend("const" = 1))]
    pub put_asset: u32,
    #[schemars(extend("const" = 1048576))]
    pub max_blob_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
/// A bounded delta page whose `from` and `next` cursors bracket `items` in order.
pub struct ReplicaChangePageV1 {
    #[schemars(extend("const" = 1))]
    pub version: u32,
    #[schemars(length(min = 1, max = 512))]
    pub from: String,
    #[schemars(length(min = 1, max = 512))]
    pub next: String,
    pub has_more: bool,
    #[schemars(length(max = 32))]
    pub items: Vec<ReplicaChangeV1>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReplicaChangeV1 {
    #[schemars(length(min = 1, max = 512))]
    pub cursor: String,
    #[serde(flatten)]
    pub projection: ReplicaProjectionV1,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
/// A bounded snapshot page. `next` continues the snapshot and `cursor` finishes it.
pub struct ReplicaSnapshotPageV1 {
    pub version: u32,
    pub snapshot_id: String,
    pub cursor: Option<String>,
    pub next: Option<String>,
    pub items: Vec<ReplicaProjectionV1>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplicaSnapshotPageWireV1 {
    version: u32,
    snapshot_id: String,
    #[serde(deserialize_with = "deserialize_required_option")]
    cursor: Option<String>,
    #[serde(deserialize_with = "deserialize_required_option")]
    next: Option<String>,
    items: Vec<ReplicaProjectionV1>,
}

pub(super) fn deserialize_required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

impl<'de> Deserialize<'de> for ReplicaSnapshotPageV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ReplicaSnapshotPageWireV1::deserialize(deserializer)?;
        if wire.cursor.is_some() == wire.next.is_some() {
            return Err(de::Error::custom("snapshot page must continue or finish"));
        }
        Ok(Self {
            version: wire.version,
            snapshot_id: wire.snapshot_id,
            cursor: wire.cursor,
            next: wire.next,
            items: wire.items,
        })
    }
}

impl JsonSchema for ReplicaSnapshotPageV1 {
    fn schema_name() -> Cow<'static, str> {
        "ReplicaSnapshotPageV1".into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let projection = generator.subschema_for::<ReplicaProjectionV1>();
        let cursor = bounded_cursor_schema();
        schemars::json_schema!({
            "type":"object",
            "required":["version","snapshot_id","cursor","next","items"],
            "properties":{
                "version":{"type":"integer","format":"uint32","minimum":0,"const":1},
                "snapshot_id":{"type":"string","minLength":1,"maxLength":4096},
                "cursor":{},
                "next":{},
                "items":{"type":"array","maxItems":32,"items":projection}
            },
            "oneOf":[
                {"properties":{"cursor":cursor.clone(),"next":{"type":"null"}}},
                {"properties":{"cursor":{"type":"null"},"next":cursor}}
            ],
            "additionalProperties":false
        })
    }
}

fn bounded_cursor_schema() -> Schema {
    schemars::json_schema!({"type":"string","minLength":1,"maxLength":512})
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(
    tag = "entity_kind",
    content = "projection",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ReplicaProjectionV1 {
    Document(DocumentRevisionV1),
    Asset(AssetRevisionV1),
    Thema(ThemaRevisionV1),
    Settings(SettingsRevisionV1),
    ReviewState(ReviewStateRevisionV1),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentOperationV1 {
    Create,
    Update,
    Rename,
    Snapshot,
    Tombstone,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetOperationV1 {
    Create,
    Update,
    Tombstone,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemaKindV1 {
    Basic,
    Cloze,
    Mcq,
    #[serde(rename = "mc-cloze")]
    McCloze,
}

impl ThemaKindV1 {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Basic => "basic",
            Self::Cloze => "cloze",
            Self::Mcq => "mcq",
            Self::McCloze => "mc-cloze",
        }
    }

    fn from_application(value: &str) -> Result<Self, ReviewAttemptWireError> {
        match value {
            "basic" => Ok(Self::Basic),
            "cloze" => Ok(Self::Cloze),
            "mcq" => Ok(Self::Mcq),
            "mc-cloze" => Ok(Self::McCloze),
            _ => Err(ReviewAttemptWireError::PresentationKind),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemaOperationV1 {
    Create,
    Update,
    Archive,
    Unarchive,
    Snapshot,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingsOperationV1 {
    Snapshot,
    Update,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentRevisionV1 {
    pub document_id: String,
    #[schemars(range(min = 1))]
    pub revision: u64,
    pub logical_path: String,
    pub content_type: String,
    pub content_sha256: Option<String>,
    pub blob_url: Option<String>,
    pub operation: DocumentOperationV1,
    pub created_at_us: u64,
    pub author_device_id: Option<String>,
    pub projection: Option<DocumentProjectionV1>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentProjectionV1 {
    pub title: String,
    pub file_node_id: Option<String>,
    pub file_tags: Vec<String>,
    pub nodes: Vec<ProjectionNodeV1>,
    pub link_occurrences: Vec<ProjectionLinkV1>,
    pub semantic_edges: Vec<ProjectionEdgeV1>,
    pub content_sha256: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectionNodeV1 {
    pub node_id: String,
    pub title: String,
    pub title_path: Vec<String>,
    pub tags: Vec<String>,
    pub parent_id: Option<String>,
    pub level: u32,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<usize>", required)]
    pub start_byte: Option<usize>,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<usize>", required)]
    pub end_byte: Option<usize>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectionLinkV1 {
    pub owner_id: String,
    pub target_id: String,
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectionEdgeV1 {
    pub source_id: String,
    pub target_id: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AssetRevisionV1 {
    pub logical_path: String,
    pub revision: u64,
    pub content_sha256: Option<String>,
    pub size_bytes: Option<u64>,
    pub media_type: Option<String>,
    pub blob_url: Option<String>,
    pub operation: AssetOperationV1,
    pub created_at_us: u64,
    pub author_device_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ThemaRevisionV1 {
    pub thema_id: String,
    pub revision: u64,
    pub kind: ThemaKindV1,
    pub keimenon: String,
    pub hypothesis: Vec<Option<String>>,
    pub answer: Vec<String>,
    pub source_guid: Option<String>,
    pub parathema: Option<String>,
    pub review_image: Option<String>,
    pub tags: Vec<String>,
    pub concepts: Vec<ThemaConceptV1>,
    pub asset_paths: Vec<String>,
    pub content_sha256: String,
    pub archived: bool,
    pub operation: ThemaOperationV1,
    pub created_at_us: u64,
    pub author_device_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsRevisionV1 {
    pub revision: u64,
    pub timezone: String,
    #[schemars(range(max = 23))]
    pub day_boundary_hour: u8,
    pub scheduler: FsrsConfigV1,
    pub operation: SettingsOperationV1,
    pub created_at_us: u64,
    pub author_device_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FsrsConfigV1 {
    pub algorithm: FsrsAlgorithmV1,
    pub model: FsrsModelV1,
    pub implementation: FsrsImplementationV1,
    pub desired_retention: f32,
    pub parameters: [f32; 21],
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FsrsAlgorithmV1 {
    Fsrs,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FsrsModelV1 {
    Fsrs6,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub enum FsrsImplementationV1 {
    #[serde(rename = "fsrs-6.6.1")]
    FsrsRs1_1,
}

impl From<&FsrsConfigSnapshot> for FsrsConfigV1 {
    fn from(value: &FsrsConfigSnapshot) -> Self {
        Self {
            algorithm: match value.algorithm {
                FsrsAlgorithmSnapshot::Fsrs => FsrsAlgorithmV1::Fsrs,
            },
            model: match value.model {
                FsrsModelSnapshot::Six => FsrsModelV1::Fsrs6,
            },
            implementation: match value.implementation {
                FsrsImplementationSnapshot::Crate6Dot6Dot1 => FsrsImplementationV1::FsrsRs1_1,
            },
            desired_retention: value.desired_retention,
            parameters: value.parameters,
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewStateRevisionV1 {
    pub thema_id: String,
    pub state: ReviewStateV1,
    pub event_id: Option<String>,
    pub created_at_us: u64,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewStateV1 {
    pub state_revision: u64,
    pub review_count: u64,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<u64>", required)]
    pub last_reviewed_at_us: Option<u64>,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<u32>", required)]
    pub last_review_day: Option<u32>,
    pub next_review_day: u32,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<u32>", required)]
    pub interval_days: Option<u32>,
    pub total_successes: u64,
    pub total_failures: u64,
    pub suspended: bool,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<f32>", required)]
    pub stability: Option<f32>,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<f32>", required)]
    pub difficulty: Option<f32>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewSourceKindV1 {
    Due,
    Overdue,
    DueWithoutOverdue,
    Tag,
    DueTag,
    Topic,
    Manual,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewSourceInputV1 {
    pub kind: ReviewSourceKindV1,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub exclude_tags: Vec<String>,
    pub topic_node_id: Option<String>,
    #[schemars(range(max = 32))]
    pub topic_forward_depth: Option<u32>,
    #[schemars(range(max = 32))]
    pub topic_backward_depth: Option<u32>,
    #[serde(default)]
    #[schemars(length(max = 500))]
    pub manual_ids: Vec<String>,
}

impl ReviewSourceInputV1 {
    pub(crate) fn into_application(self) -> ReviewSource {
        ReviewSource {
            kind: self.kind.into(),
            tags: self.tags,
            exclude_tags: self.exclude_tags,
            topic_node_id: self.topic_node_id,
            topic_forward_depth: self.topic_forward_depth,
            topic_backward_depth: self.topic_backward_depth,
            manual_ids: self.manual_ids,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewSourceV1 {
    pub kind: ReviewSourceKindV1,
    pub tags: Vec<String>,
    pub exclude_tags: Vec<String>,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<String>", required)]
    pub topic_node_id: Option<String>,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<u32>", required)]
    #[schemars(range(max = 32))]
    pub topic_forward_depth: Option<u32>,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<u32>", required)]
    #[schemars(range(max = 32))]
    pub topic_backward_depth: Option<u32>,
    #[schemars(length(max = 500))]
    pub manual_ids: Vec<String>,
}

impl From<&ReviewSource> for ReviewSourceV1 {
    fn from(value: &ReviewSource) -> Self {
        Self {
            kind: (&value.kind).into(),
            tags: value.tags.clone(),
            exclude_tags: value.exclude_tags.clone(),
            topic_node_id: value.topic_node_id.clone(),
            topic_forward_depth: value.topic_forward_depth,
            topic_backward_depth: value.topic_backward_depth,
            manual_ids: value.manual_ids.clone(),
        }
    }
}

impl From<&ReviewSourceKind> for ReviewSourceKindV1 {
    fn from(value: &ReviewSourceKind) -> Self {
        match value {
            ReviewSourceKind::Due => Self::Due,
            ReviewSourceKind::Overdue => Self::Overdue,
            ReviewSourceKind::DueWithoutOverdue => Self::DueWithoutOverdue,
            ReviewSourceKind::Tag => Self::Tag,
            ReviewSourceKind::DueTag => Self::DueTag,
            ReviewSourceKind::Topic => Self::Topic,
            ReviewSourceKind::Manual => Self::Manual,
        }
    }
}

impl From<ReviewSourceKindV1> for ReviewSourceKind {
    fn from(value: ReviewSourceKindV1) -> Self {
        match value {
            ReviewSourceKindV1::Due => Self::Due,
            ReviewSourceKindV1::Overdue => Self::Overdue,
            ReviewSourceKindV1::DueWithoutOverdue => Self::DueWithoutOverdue,
            ReviewSourceKindV1::Tag => Self::Tag,
            ReviewSourceKindV1::DueTag => Self::DueTag,
            ReviewSourceKindV1::Topic => Self::Topic,
            ReviewSourceKindV1::Manual => Self::Manual,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewSessionRequestV1 {
    pub source: ReviewSourceInputV1,
    #[schemars(range(min = 1, max = 500))]
    #[schemars(extend("default" = 100))]
    pub target_count: Option<u32>,
}

impl ReviewSessionRequestV1 {
    pub(crate) fn into_application(self) -> (ReviewSource, Option<u32>) {
        (self.source.into_application(), self.target_count)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewSessionModeV1 {
    Online,
    Offline,
}

impl ReviewSessionModeV1 {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Online => "online",
            Self::Offline => "offline",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewSessionStatusV1 {
    Active,
    Completed,
    Quit,
    Aborted,
}

impl ReviewSessionStatusV1 {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Completed => "completed",
            Self::Quit => "quit",
            Self::Aborted => "aborted",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewSessionV1 {
    pub session_id: String,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<String>", required)]
    pub owner_device_id: Option<String>,
    pub mode: ReviewSessionModeV1,
    pub source: ReviewSourceV1,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<u32>", required)]
    pub target_count: Option<u32>,
    pub status: ReviewSessionStatusV1,
    pub current_position: u32,
    pub queue_length: u32,
    pub started_at_us: u64,
}

impl ReviewSessionV1 {
    pub(crate) fn from_application(
        value: &ReviewSessionView,
    ) -> Result<Self, ReviewAttemptWireError> {
        Ok(Self {
            session_id: value.session_id.clone(),
            owner_device_id: value.owner_device_id.clone(),
            mode: value.mode.into(),
            source: (&value.source).into(),
            target_count: value.target_count,
            status: ReviewSessionStatusV1::from_application(&value.status)?,
            current_position: value.current_position,
            queue_length: value.queue_length,
            started_at_us: value.started_at_us,
        })
    }
}

impl From<ReviewSessionMode> for ReviewSessionModeV1 {
    fn from(value: ReviewSessionMode) -> Self {
        match value {
            ReviewSessionMode::Online => Self::Online,
            ReviewSessionMode::Offline => Self::Offline,
        }
    }
}

impl ReviewSessionStatusV1 {
    fn from_application(value: &str) -> Result<Self, ReviewAttemptWireError> {
        match value {
            "active" => Ok(Self::Active),
            "completed" => Ok(Self::Completed),
            "quit" => Ok(Self::Quit),
            "aborted" => Ok(Self::Aborted),
            _ => Err(ReviewAttemptWireError::SessionStatus),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewItemSkipReasonV1 {
    Archived,
    Suspended,
    ContentChanged,
    SchedulingChanged,
    UnsafePresentation,
}

impl From<ReviewItemSkipReason> for ReviewItemSkipReasonV1 {
    fn from(value: ReviewItemSkipReason) -> Self {
        match value {
            ReviewItemSkipReason::Archived => Self::Archived,
            ReviewItemSkipReason::Suspended => Self::Suspended,
            ReviewItemSkipReason::ContentChanged => Self::ContentChanged,
            ReviewItemSkipReason::SchedulingChanged => Self::SchedulingChanged,
            ReviewItemSkipReason::UnsafePresentation => Self::UnsafePresentation,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkippedReviewItemV1 {
    pub position: u32,
    pub thema_id: String,
    pub reason: ReviewItemSkipReasonV1,
}

impl From<&SkippedReviewItem> for SkippedReviewItemV1 {
    fn from(value: &SkippedReviewItem) -> Self {
        Self {
            position: value.position,
            thema_id: value.thema_id.clone(),
            reason: value.reason.into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewAttemptBeginResponseV1 {
    pub start_position: u32,
    pub session: ReviewSessionV1,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<ReviewAttemptV1>", required)]
    pub attempt: Option<ReviewAttemptV1>,
    pub skipped: Vec<SkippedReviewItemV1>,
}

impl ReviewAttemptBeginResponseV1 {
    pub(crate) fn from_application(
        value: &ReviewAttemptBeginResult,
    ) -> Result<Self, ReviewAttemptWireError> {
        Ok(Self {
            start_position: value.start_position,
            session: ReviewSessionV1::from_application(&value.session)?,
            attempt: value
                .attempt
                .as_ref()
                .map(ReviewAttemptV1::from_application)
                .transpose()?,
            skipped: value.skipped.iter().map(Into::into).collect(),
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveReviewSessionV1 {
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<ReviewSessionV1>", required)]
    pub session: Option<ReviewSessionV1>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewCommitResponseV1 {
    pub attempt_id: String,
    pub event_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetAcceptedSuccessRequestV1 {
    #[schemars(extend("const" = 1))]
    pub version: u32,
    #[schemars(
        length(min = 1, max = 128),
        regex(pattern = "^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$")
    )]
    pub operation_id: String,
    #[schemars(range(max = 9223372036854775807_u64))]
    pub expected_attempt_revision: u64,
    pub accepted_success: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetSuspensionRequestV1 {
    #[schemars(extend("const" = 1))]
    pub version: u32,
    #[schemars(
        length(min = 1, max = 128),
        regex(pattern = "^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$")
    )]
    pub operation_id: String,
    #[schemars(range(max = 9223372036854775807_u64))]
    pub expected_attempt_revision: u64,
    pub desired_suspended: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetThemaSuspensionRequestV1 {
    pub suspended: bool,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewAttemptOperationResponseV1 {
    #[schemars(extend("const" = 1))]
    pub version: u32,
    #[schemars(
        length(min = 1, max = 128),
        regex(pattern = "^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$")
    )]
    pub operation_id: String,
    pub changed: bool,
    pub attempt_revision: u64,
    pub attempt: ReviewAttemptV1,
}

impl ReviewAttemptOperationResponseV1 {
    pub(crate) fn from_application(
        value: &ReviewAttemptOperationResult,
    ) -> Result<Self, ReviewAttemptWireError> {
        Ok(Self {
            version: 1,
            operation_id: value.operation_id.clone(),
            changed: value.changed,
            attempt_revision: value.attempt_revision,
            attempt: ReviewAttemptV1::from_application(&value.attempt)?,
        })
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewAttemptStateResponseV1 {
    #[schemars(extend("const" = 1))]
    pub version: u32,
    pub attempt_revision: u64,
    pub attempt: ReviewAttemptV1,
}

impl ReviewAttemptStateResponseV1 {
    pub(crate) fn from_application(
        value: &ReviewAttemptState,
    ) -> Result<Self, ReviewAttemptWireError> {
        Ok(Self {
            version: 1,
            attempt_revision: value.attempt_revision,
            attempt: ReviewAttemptV1::from_application(&value.attempt)?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewAttemptV1 {
    pub attempt_id: String,
    pub session_id: String,
    pub thema_id: String,
    pub presentation: ReviewPresentationV1,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub revealed_answer: Option<Vec<String>>,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub revealed_parathema: Option<String>,
    pub matcher_version: u32,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub response: Option<ReviewAnswerV1>,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub detected_success: Option<bool>,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub accepted_success: Option<bool>,
    pub override_count: u32,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub proposed_transition: Option<ReviewTransitionV1>,
    pub desired_suspended: bool,
    pub status: ReviewAttemptStatusV1,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReviewAttemptWireV1 {
    attempt_id: String,
    session_id: String,
    thema_id: String,
    presentation: ReviewPresentationV1,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<Vec<String>>", required)]
    revealed_answer: Option<Vec<String>>,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<String>", required)]
    revealed_parathema: Option<String>,
    matcher_version: u32,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<ReviewAnswerV1>", required)]
    response: Option<ReviewAnswerV1>,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<bool>", required)]
    detected_success: Option<bool>,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<bool>", required)]
    accepted_success: Option<bool>,
    override_count: u32,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(
        with = "crate::protocol::RequiredNullable<ReviewTransitionV1>",
        required
    )]
    proposed_transition: Option<ReviewTransitionV1>,
    desired_suspended: bool,
    status: ReviewAttemptStatusV1,
}

impl From<ReviewAttemptWireV1> for ReviewAttemptV1 {
    fn from(value: ReviewAttemptWireV1) -> Self {
        Self {
            attempt_id: value.attempt_id,
            session_id: value.session_id,
            thema_id: value.thema_id,
            presentation: value.presentation,
            revealed_answer: value.revealed_answer,
            revealed_parathema: value.revealed_parathema,
            matcher_version: value.matcher_version,
            response: value.response,
            detected_success: value.detected_success,
            accepted_success: value.accepted_success,
            override_count: value.override_count,
            proposed_transition: value.proposed_transition,
            desired_suspended: value.desired_suspended,
            status: value.status,
        }
    }
}

impl<'de> Deserialize<'de> for ReviewAttemptV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let attempt = Self::from(ReviewAttemptWireV1::deserialize(deserializer)?);
        if !attempt.has_valid_status_fields() || !attempt.has_valid_presentation_fields() {
            return Err(de::Error::custom(
                "review attempt fields do not match its status or presentation",
            ));
        }
        Ok(attempt)
    }
}

impl JsonSchema for ReviewAttemptV1 {
    fn schema_name() -> Cow<'static, str> {
        "ReviewAttemptV1".into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let mut schema = ReviewAttemptWireV1::json_schema(generator);
        let response = generator.subschema_for::<ReviewAnswerV1>().to_value();
        let transition = generator.subschema_for::<ReviewTransitionV1>().to_value();
        let pending_presentation = review_attempt_presentation_schema(false);
        let graded_presentation = review_attempt_presentation_schema(true);
        let graded_fields = serde_json::json!({
            "revealed_answer":{"type":"array","items":{"type":"string"}},
            "response":response,
            "detected_success":{"type":"boolean"},
            "accepted_success":{"type":"boolean"},
            "proposed_transition":transition
        });
        schema.insert(
            "oneOf".into(),
            serde_json::json!([
                {"properties":{
                    "status":{"const":"pending"},
                    "presentation":pending_presentation,
                    "revealed_answer":{"type":"null"},
                    "revealed_parathema":{"type":"null"},
                    "response":{"type":"null"},
                    "detected_success":{"type":"null"},
                    "accepted_success":{"type":"null"},
                    "override_count":{"const":0},
                    "proposed_transition":{"type":"null"}
                }},
                {"properties":{
                    "status":{"const":"graded"},
                    "presentation":graded_presentation.clone(),
                    "revealed_answer":graded_fields["revealed_answer"].clone(),
                    "response":graded_fields["response"].clone(),
                    "detected_success":graded_fields["detected_success"].clone(),
                    "accepted_success":graded_fields["accepted_success"].clone(),
                    "proposed_transition":graded_fields["proposed_transition"].clone()
                }},
                {"properties":{
                    "status":{"const":"committed"},
                    "presentation":graded_presentation,
                    "revealed_answer":graded_fields["revealed_answer"].clone(),
                    "response":graded_fields["response"].clone(),
                    "detected_success":graded_fields["detected_success"].clone(),
                    "accepted_success":graded_fields["accepted_success"].clone(),
                    "proposed_transition":graded_fields["proposed_transition"].clone()
                }}
            ]),
        );
        schema
    }
}

fn review_attempt_presentation_schema(graded: bool) -> serde_json::Value {
    let cloze_states = if graded {
        serde_json::json!({"enum":["correct","incorrect","unanswered"]})
    } else {
        serde_json::json!({"const":"concealed"})
    };
    let mc_cloze_states = if graded {
        serde_json::json!({"enum":["correct","incorrect"]})
    } else {
        serde_json::json!({"const":"concealed"})
    };
    serde_json::json!({
        "oneOf":[
            review_attempt_presentation_case(
                "basic", "text", serde_json::json!({"maxItems":0})),
            review_attempt_presentation_case(
                "cloze", "texts",
                serde_json::json!({
                    "minItems":1,
                    "items":{"properties":{"state":cloze_states}}
                })),
            review_attempt_presentation_case(
                "mcq", "choice", serde_json::json!({"maxItems":0})),
            review_attempt_presentation_case(
                "mc-cloze", "choice",
                serde_json::json!({
                    "minItems":1,
                    "maxItems":1,
                    "items":{"properties":{"state":mc_cloze_states}}
                }))
        ]
    })
}

fn review_attempt_presentation_case(
    kind: &str,
    response_kind: &str,
    cloze_spans: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "properties":{
            "kind":{"const":kind},
            "response":{"properties":{"kind":{"const":response_kind}}},
            "cloze_spans":cloze_spans
        }
    })
}

impl ReviewAttemptV1 {
    pub(crate) fn has_valid_status_fields(&self) -> bool {
        match self.status {
            ReviewAttemptStatusV1::Pending => {
                self.revealed_answer.is_none()
                    && self.revealed_parathema.is_none()
                    && self.response.is_none()
                    && self.detected_success.is_none()
                    && self.accepted_success.is_none()
                    && self.override_count == 0
                    && self.proposed_transition.is_none()
            }
            ReviewAttemptStatusV1::Graded | ReviewAttemptStatusV1::Committed => {
                self.revealed_answer.is_some()
                    && self.response.is_some()
                    && self.detected_success.is_some()
                    && self.accepted_success.is_some()
                    && self.proposed_transition.is_some()
            }
        }
    }

    pub(crate) fn has_valid_presentation_fields(&self) -> bool {
        let response_matches = match (self.presentation.kind, self.response.as_ref()) {
            (_, None) => self.status == ReviewAttemptStatusV1::Pending,
            (ThemaKindV1::Basic, Some(ReviewAnswerV1::Text(_)))
            | (ThemaKindV1::Cloze, Some(ReviewAnswerV1::Texts(_)))
            | (ThemaKindV1::Mcq | ThemaKindV1::McCloze, Some(ReviewAnswerV1::Choice(_))) => {
                self.status != ReviewAttemptStatusV1::Pending
            }
            _ => false,
        };
        response_matches
            && self.presentation.has_valid_cloze_spans(self.status)
            && self.revealed_answer.as_ref().is_none_or(|answers| {
                let expected = match self.presentation.kind {
                    ThemaKindV1::Cloze => self.presentation.cloze_spans.len(),
                    _ => 1,
                };
                answers.len() == expected
            })
    }

    pub(crate) fn from_application(
        value: &ReviewAttemptView,
    ) -> Result<Self, ReviewAttemptWireError> {
        let reveal = value.response.is_some();
        let attempt = Self {
            attempt_id: value.attempt_id.clone(),
            session_id: value.session_id.clone(),
            thema_id: value.thema_id.clone(),
            presentation: ReviewPresentationV1::from_application(value)?,
            revealed_answer: reveal.then(|| value.presentation.answer.clone()),
            revealed_parathema: reveal
                .then(|| value.presentation.parathema.clone())
                .flatten(),
            matcher_version: value.matcher_version,
            response: value.response.as_ref().map(ReviewAnswerV1::from),
            detected_success: value.detected_success,
            accepted_success: value.accepted_success,
            override_count: value.override_count,
            proposed_transition: value
                .proposed_transition
                .as_ref()
                .map(ReviewTransitionV1::from),
            desired_suspended: value.desired_suspended,
            status: ReviewAttemptStatusV1::from_application(&value.status)?,
        };
        if !attempt.has_valid_status_fields() || !attempt.has_valid_presentation_fields() {
            return Err(ReviewAttemptWireError::StatusFields);
        }
        Ok(attempt)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewAttemptStatusV1 {
    Pending,
    Graded,
    Committed,
}

impl ReviewAttemptStatusV1 {
    fn from_application(value: &str) -> Result<Self, ReviewAttemptWireError> {
        match value {
            "pending" => Ok(Self::Pending),
            "graded" => Ok(Self::Graded),
            "committed" => Ok(Self::Committed),
            _ => Err(ReviewAttemptWireError::Status),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewPresentationV1 {
    pub kind: ThemaKindV1,
    #[schemars(length(min = 1))]
    pub question: String,
    pub cloze_spans: Vec<ReviewClozeSpanV1>,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<String>", required)]
    pub hint: Option<String>,
    pub response: ReviewResponsePromptV1,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<String>", required)]
    pub source_guid: Option<String>,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<String>", required)]
    pub review_image: Option<String>,
    pub tags: Vec<String>,
}

impl ReviewPresentationV1 {
    fn from_application(value: &ReviewAttemptView) -> Result<Self, ReviewAttemptWireError> {
        let question =
            review_attempt_question(value).map_err(|_| ReviewAttemptWireError::Presentation)?;
        Self::from_question(&value.presentation, question)
    }

    fn from_question(
        presentation: &PresentationSnapshot,
        question: ReviewQuestion,
    ) -> Result<Self, ReviewAttemptWireError> {
        Ok(Self {
            kind: ThemaKindV1::from_application(&presentation.kind)?,
            question: question.text,
            cloze_spans: question
                .cloze_spans
                .iter()
                .map(ReviewClozeSpanV1::from)
                .collect(),
            hint: question.hint,
            response: ReviewResponsePromptV1::from_application(question.response),
            source_guid: presentation.source_guid.clone(),
            review_image: presentation.review_image.clone(),
            tags: presentation.tags.clone(),
        })
    }

    fn has_valid_cloze_spans(&self, status: ReviewAttemptStatusV1) -> bool {
        !self.question.trim().is_empty()
            && match (&self.kind, &self.response) {
                (ThemaKindV1::Basic, ReviewResponsePromptV1::Text { .. })
                | (ThemaKindV1::Mcq, ReviewResponsePromptV1::Choice { .. }) => {
                    self.cloze_spans.is_empty()
                }
                (ThemaKindV1::Cloze, ReviewResponsePromptV1::Texts { input_scripts }) => {
                    self.cloze_spans.len() == input_scripts.len()
                        && valid_cloze_span_ranges(&self.question, &self.cloze_spans, status, false)
                }
                (ThemaKindV1::McCloze, ReviewResponsePromptV1::Choice { .. }) => {
                    self.cloze_spans.len() == 1
                        && valid_cloze_span_ranges(&self.question, &self.cloze_spans, status, true)
                }
                _ => false,
            }
    }
}

fn valid_cloze_span_ranges(
    question: &str,
    spans: &[ReviewClozeSpanV1],
    status: ReviewAttemptStatusV1,
    multiple_choice: bool,
) -> bool {
    let mut previous_end = 0;
    !question.trim().is_empty()
        && !spans.is_empty()
        && spans.iter().all(|span| {
            let state_is_valid = match status {
                ReviewAttemptStatusV1::Pending => span.state == ReviewClozeStateV1::Concealed,
                ReviewAttemptStatusV1::Graded | ReviewAttemptStatusV1::Committed => {
                    matches!(
                        span.state,
                        ReviewClozeStateV1::Correct | ReviewClozeStateV1::Incorrect
                    ) || (!multiple_choice && span.state == ReviewClozeStateV1::Unanswered)
                }
            };
            let range_is_valid = previous_end <= span.start_byte
                && span.start_byte < span.end_byte
                && span.end_byte <= question.len()
                && question.is_char_boundary(span.start_byte)
                && question.is_char_boundary(span.end_byte);
            previous_end = span.end_byte;
            state_is_valid && range_is_valid
        })
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewClozeSpanV1 {
    pub start_byte: usize,
    pub end_byte: usize,
    pub state: ReviewClozeStateV1,
}

impl From<&ReviewClozeSpan> for ReviewClozeSpanV1 {
    fn from(value: &ReviewClozeSpan) -> Self {
        Self {
            start_byte: value.start_byte,
            end_byte: value.end_byte,
            state: ReviewClozeStateV1::from(value.state),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewClozeStateV1 {
    Concealed,
    Correct,
    Incorrect,
    Unanswered,
}

impl From<ReviewClozeState> for ReviewClozeStateV1 {
    fn from(value: ReviewClozeState) -> Self {
        match value {
            ReviewClozeState::Concealed => Self::Concealed,
            ReviewClozeState::Correct => Self::Correct,
            ReviewClozeState::Incorrect => Self::Incorrect,
            ReviewClozeState::Unanswered => Self::Unanswered,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReviewResponsePromptV1 {
    Text {
        #[serde(deserialize_with = "deserialize_required_option")]
        #[schemars(with = "crate::protocol::RequiredNullable<String>", required)]
        input_script: Option<String>,
    },
    Texts {
        #[schemars(schema_with = "review_cloze_input_scripts_schema")]
        input_scripts: Vec<Option<String>>,
    },
    Choice {
        #[schemars(length(min = 2))]
        choices: Vec<String>,
    },
}

fn review_cloze_input_scripts_schema(generator: &mut SchemaGenerator) -> Schema {
    let mut schema = Vec::<Option<String>>::json_schema(generator);
    schema.insert("minItems".into(), 1.into());
    schema.insert(
        "maxItems".into(),
        crate::domain::MAX_REVIEW_CLOZE_INPUTS.into(),
    );
    schema
}

impl ReviewResponsePromptV1 {
    fn from_application(value: ReviewResponsePrompt) -> Self {
        match value {
            ReviewResponsePrompt::Text { input_script } => Self::Text { input_script },
            ReviewResponsePrompt::Texts { input_scripts } => Self::Texts { input_scripts },
            ReviewResponsePrompt::Choice { choices } => Self::Choice { choices },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReviewAttemptWireError {
    Presentation,
    PresentationKind,
    SessionStatus,
    Status,
    StatusFields,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ReviewAnswerV1 {
    Text(String),
    Texts(#[schemars(length(max = 500))] Vec<String>),
    Choice(String),
}

impl From<&AnswerInput> for ReviewAnswerV1 {
    fn from(value: &AnswerInput) -> Self {
        match value {
            AnswerInput::Text(value) => Self::Text(value.clone()),
            AnswerInput::Texts(value) => Self::Texts(value.clone()),
            AnswerInput::Choice(value) => Self::Choice(value.clone()),
        }
    }
}

impl From<ReviewAnswerV1> for AnswerInput {
    fn from(value: ReviewAnswerV1) -> Self {
        match value {
            ReviewAnswerV1::Text(value) => Self::Text(value),
            ReviewAnswerV1::Texts(value) => Self::Texts(value),
            ReviewAnswerV1::Choice(value) => Self::Choice(value),
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewTransitionV1 {
    pub before: ReviewStateV1,
    pub after: ReviewStateV1,
    pub config: FsrsConfigV1,
    pub reviewed_at_us: u64,
    pub review_day: u32,
    pub outcome: RecallOutcomeV1,
    pub first_review: bool,
    pub elapsed_days: u32,
    pub raw_interval_days: f32,
    pub interval_days: u32,
    pub due_day: u32,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecallOutcomeV1 {
    Failure,
    Success,
}

impl From<&TransitionSnapshot> for ReviewTransitionV1 {
    fn from(value: &TransitionSnapshot) -> Self {
        Self {
            before: ReviewStateV1::from(&value.before),
            after: ReviewStateV1::from(&value.after),
            config: FsrsConfigV1::from(&value.config),
            reviewed_at_us: value.reviewed_at_us,
            review_day: value.review_day,
            outcome: match value.outcome {
                RecallOutcomeSnapshot::Failure => RecallOutcomeV1::Failure,
                RecallOutcomeSnapshot::Success => RecallOutcomeV1::Success,
            },
            first_review: value.first_review,
            elapsed_days: value.elapsed_days,
            raw_interval_days: value.raw_interval_days,
            interval_days: value.interval_days,
            due_day: value.due_day,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiErrorCodeV1 {
    Unauthorized,
    Forbidden,
    NotFound,
    InvalidRequest,
    PreconditionRequired,
    Internal,
    InvalidContent,
    RequestTooLarge,
    InvalidBody,
    RequestTimeout,
    CatalogChanged,
    InvalidServerState,
    UnsupportedOfflineReviewVersion,
    InvalidOfflineReviewOperation,
    UnsupportedOfflineContentVersion,
    InvalidOfflineContentOperation,
    UnsupportedOfflineAssetVersion,
    InvalidOfflineAssetOperation,
    ReplicaProjectionTooLarge,
    SnapshotRequired,
    RevisionConflict,
    Busy,
    ActiveSession,
    StaleAttempt,
    ReviewStateConflict,
    IdempotencyConflict,
    DeviceSequenceGap,
    BlobRequired,
    BlobMetadataConflict,
    RenameRequired,
    InvalidBlobMetadata,
}

impl ApiErrorCodeV1 {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unauthorized => "unauthorized",
            Self::Forbidden => "forbidden",
            Self::NotFound => "not_found",
            Self::InvalidRequest => "invalid_request",
            Self::PreconditionRequired => "precondition_required",
            Self::Internal => "internal",
            Self::InvalidContent => "invalid_content",
            Self::RequestTooLarge => "request_too_large",
            Self::InvalidBody => "invalid_body",
            Self::RequestTimeout => "request_timeout",
            Self::CatalogChanged => "catalog_changed",
            Self::InvalidServerState => "invalid_server_state",
            Self::UnsupportedOfflineReviewVersion => "unsupported_offline_review_version",
            Self::InvalidOfflineReviewOperation => "invalid_offline_review_operation",
            Self::UnsupportedOfflineContentVersion => "unsupported_offline_content_version",
            Self::InvalidOfflineContentOperation => "invalid_offline_content_operation",
            Self::UnsupportedOfflineAssetVersion => "unsupported_offline_asset_version",
            Self::InvalidOfflineAssetOperation => "invalid_offline_asset_operation",
            Self::ReplicaProjectionTooLarge => "replica_projection_too_large",
            Self::SnapshotRequired => "snapshot_required",
            Self::RevisionConflict => "revision_conflict",
            Self::Busy => "busy",
            Self::ActiveSession => "active_session",
            Self::StaleAttempt => "stale_attempt",
            Self::ReviewStateConflict => "review_state_conflict",
            Self::IdempotencyConflict => "idempotency_conflict",
            Self::DeviceSequenceGap => "device_sequence_gap",
            Self::BlobRequired => "blob_required",
            Self::BlobMetadataConflict => "blob_metadata_conflict",
            Self::RenameRequired => "rename_required",
            Self::InvalidBlobMetadata => "invalid_blob_metadata",
        }
    }
}

const METADATA_ERROR_CODES: [ApiErrorCodeV1; 4] = [
    ApiErrorCodeV1::SnapshotRequired,
    ApiErrorCodeV1::RevisionConflict,
    ApiErrorCodeV1::DeviceSequenceGap,
    ApiErrorCodeV1::BlobRequired,
];

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum ApiErrorMetaV1 {
    Snapshot {
        snapshot_url: String,
    },
    Revision {
        expected_revision: u64,
        #[serde(deserialize_with = "deserialize_required_option")]
        #[schemars(with = "crate::protocol::RequiredNullable<u64>", required)]
        current_revision: Option<u64>,
    },
    DeviceSequence {
        expected_sequence: u64,
    },
    Blob {
        content_sha256: String,
        upload_url: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
/// A client-visible error whose metadata variant is determined by `code`.
pub struct ApiErrorBodyV1 {
    code: ApiErrorCodeV1,
    message: String,
    meta: Option<ApiErrorMetaV1>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApiErrorBodyWireV1 {
    code: ApiErrorCodeV1,
    message: String,
    #[serde(deserialize_with = "deserialize_required_option")]
    meta: Option<ApiErrorMetaV1>,
}

impl ApiErrorBodyV1 {
    pub(crate) fn new(code: ApiErrorCodeV1, message: String, meta: Option<ApiErrorMetaV1>) -> Self {
        assert!(error_metadata_matches(code, meta.as_ref()));
        Self {
            code,
            message,
            meta,
        }
    }

    pub const fn code(&self) -> ApiErrorCodeV1 {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub const fn meta(&self) -> Option<&ApiErrorMetaV1> {
        self.meta.as_ref()
    }
}

impl<'de> Deserialize<'de> for ApiErrorBodyV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ApiErrorBodyWireV1::deserialize(deserializer)?;
        if !error_metadata_matches(wire.code, wire.meta.as_ref()) {
            return Err(de::Error::custom("error metadata does not match its code"));
        }
        Ok(Self {
            code: wire.code,
            message: wire.message,
            meta: wire.meta,
        })
    }
}

impl JsonSchema for ApiErrorBodyV1 {
    fn schema_name() -> Cow<'static, str> {
        "ApiErrorBodyV1".into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        error_schema::json_schema(generator)
    }
}

fn error_metadata_matches(code: ApiErrorCodeV1, meta: Option<&ApiErrorMetaV1>) -> bool {
    match code {
        ApiErrorCodeV1::SnapshotRequired => matches!(
            meta,
            Some(ApiErrorMetaV1::Snapshot { snapshot_url })
                if snapshot_url == "/v1/sync/snapshot"
        ),
        ApiErrorCodeV1::RevisionConflict => matches!(meta, Some(ApiErrorMetaV1::Revision { .. })),
        ApiErrorCodeV1::DeviceSequenceGap => {
            matches!(meta, Some(ApiErrorMetaV1::DeviceSequence { .. }))
        }
        ApiErrorCodeV1::BlobRequired => matches!(
            meta,
            Some(ApiErrorMetaV1::Blob {
                content_sha256,
                upload_url,
            }) if crate::is_canonical_sha256(content_sha256)
                && upload_url == &format!("/v1/sync/blobs/{content_sha256}")
        ),
        _ => meta.is_none(),
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApiErrorResponseV1 {
    pub error: ApiErrorBodyV1,
}

mod error_schema {
    #![allow(dead_code)]

    use schemars::{JsonSchema, Schema, SchemaGenerator};

    use super::{ApiErrorCodeV1, METADATA_ERROR_CODES};

    pub(super) fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        ApiErrorBodySchemaV1::json_schema(generator)
    }

    // These types exist only to express code-dependent metadata in OpenAPI.
    #[derive(JsonSchema)]
    #[serde(untagged)]
    enum ApiErrorBodySchemaV1 {
        Plain(PlainApiErrorSchemaV1),
        Snapshot(SnapshotApiErrorSchemaV1),
        Revision(RevisionApiErrorSchemaV1),
        DeviceSequence(DeviceSequenceApiErrorSchemaV1),
        Blob(BlobApiErrorSchemaV1),
    }

    #[derive(JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct PlainApiErrorSchemaV1 {
        code: PlainApiErrorCodeSchemaV1,
        message: String,
        meta: (),
    }

    struct PlainApiErrorCodeSchemaV1;

    impl JsonSchema for PlainApiErrorCodeSchemaV1 {
        fn schema_name() -> std::borrow::Cow<'static, str> {
            "PlainApiErrorCodeSchemaV1".into()
        }

        fn json_schema(generator: &mut SchemaGenerator) -> Schema {
            let canonical = generator.subschema_for::<ApiErrorCodeV1>();
            let metadata_codes = serde_json::to_value(METADATA_ERROR_CODES).unwrap();
            schemars::json_schema!({
                "allOf":[canonical,{"not":{"enum":metadata_codes}}]
            })
        }
    }

    #[derive(JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct SnapshotApiErrorSchemaV1 {
        #[schemars(extend("const" = "snapshot_required"))]
        code: String,
        message: String,
        meta: SnapshotErrorMetaSchemaV1,
    }

    #[derive(JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct SnapshotErrorMetaSchemaV1 {
        #[schemars(extend("const" = "/v1/sync/snapshot"))]
        snapshot_url: String,
    }

    #[derive(JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct RevisionApiErrorSchemaV1 {
        #[schemars(extend("const" = "revision_conflict"))]
        code: String,
        message: String,
        meta: RevisionErrorMetaSchemaV1,
    }

    #[derive(JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct RevisionErrorMetaSchemaV1 {
        expected_revision: u64,
        #[schemars(with = "crate::protocol::RequiredNullable<u64>", required)]
        current_revision: Option<u64>,
    }

    #[derive(JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct DeviceSequenceApiErrorSchemaV1 {
        #[schemars(extend("const" = "device_sequence_gap"))]
        code: String,
        message: String,
        meta: DeviceSequenceErrorMetaSchemaV1,
    }

    #[derive(JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct DeviceSequenceErrorMetaSchemaV1 {
        expected_sequence: u64,
    }

    #[derive(JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct BlobApiErrorSchemaV1 {
        #[schemars(extend("const" = "blob_required"))]
        code: String,
        message: String,
        meta: BlobErrorMetaSchemaV1,
    }

    #[derive(JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct BlobErrorMetaSchemaV1 {
        #[schemars(regex(pattern = "^[0-9a-f]{64}$"))]
        content_sha256: String,
        #[schemars(regex(pattern = "^/v1/sync/blobs/[0-9a-f]{64}$"))]
        upload_url: String,
    }
}
